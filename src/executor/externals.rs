// Substrate-lite
// Copyright (C) 2019-2020  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Wasm virtual machine specific to the Substrate/Polkadot Runtime Environment.
//!
//! Contrary to [`VirtualMachine`](super::vm::VirtualMachine), this code is not just a generic
//! Wasm virtual machine, but is aware of the Substrate/Polkadot runtime environment. The external
//! functions that the Wasm code calls are automatically resolved and either handled or notified
//! to the user of this module.
//!
//! Any external function that requires pure CPU computations (for example building or verifying
//! a cryptographic signature) is directly handled by the code in this module. Other external
//! functions (for example accessing the state or printing a message) are instead handled by
//! interrupting the virtual machine and waiting for the user of this module to handle the call.
//!
//! > **Note**: The `ext_offchain_random_seed_version_1` and `ext_offchain_timestamp_version_1`
//! >           functions, which requires the host to respectively produce a random seed and
//! >           return the current time, must also be handled by the user. While these functions
//! >           could theoretically be handled directly by this module, it might be useful for
//! >           testing purposes to have the possibility to return a deterministic value.
//!
//! Contrary to most programs, Wasm runtime code doesn't have a singe `main` function. Instead, it
//! exposes several entry points. Which one to call indicates which action it has to perform. Not
//! all entry points are necessarily available on all runtimes.
//!
//! # ABI
//!
//! All entry points have the same signature:
//!
//! ```ignore
//! (func $runtime_entry(param $data i32) (param $len i32) (result i64))
//! ```
//!
//! In order to call into the runtime, one must write a buffer of data containing the input
//! parameters into the Wasm virtual machine's memory, then pass a pointer and length of this
//! buffer as the parameters of the entry point.
//!
//! The function returns a 64bits number. The 32 less significant bits represent a pointer to the
//! Wasm virtual machine's memory, and the 32 most significant bits a length. This pointer and
//! length designate a buffer containing the actual return value.

use super::{allocator, vm};
use crate::util;

use alloc::{format, string::String, vec::Vec};
use core::{convert::TryFrom as _, fmt, hash::Hasher as _, iter};
use parity_scale_codec::DecodeAll as _;
use sha2::Digest as _;
use tiny_keccak::Hasher as _;

/// Prototype for an [`ExternalsVm`].
pub struct ExternalsVmPrototype {
    /// Inner virtual machine prototype.
    vm_proto: vm::VirtualMachinePrototype,

    /// Initial value of the `__heap_base` global in the Wasm module. Used to initialize the memory
    /// allocator.
    heap_base: u32,

    /// List of functions that the Wasm code imports.
    ///
    /// The keys of this `Vec` (i.e. the `usize` indices) have been passed to the virtual machine
    /// executor. Whenever the Wasm code invokes an external function, we obtain its index, and
    /// look within this `Vec` to know what to do.
    registered_functions: Vec<Externality>,
}

impl ExternalsVmPrototype {
    /// Creates a new [`ExternalsVmPrototype`]. Parses and potentially JITs the module.
    // TODO: document `heap_pages`; I know it comes from storage, but it's unclear what it means exactly
    pub fn new(
        module: impl AsRef<[u8]>,
        heap_pages: u64,
        exec_hint: vm::ExecHint,
    ) -> Result<Self, NewErr> {
        // Initialize the virtual machine.
        // Each symbol requested by the Wasm runtime will be put in `registered_functions`. Later,
        // when a function is invoked, the Wasm virtual machine will pass indices within that
        // array.
        let (mut vm_proto, registered_functions) = {
            let mut registered_functions = Vec::new();
            let vm_proto = vm::VirtualMachinePrototype::new(
                module,
                heap_pages,
                exec_hint,
                // This closure is called back for each function that the runtime imports.
                |mod_name, f_name, _signature| {
                    if mod_name != "env" {
                        return Err(());
                    }

                    let id = registered_functions.len();
                    registered_functions.push(match Externality::by_name(f_name) {
                        Some(f) => f,
                        None => return Err(()),
                    });
                    Ok(id)
                },
            )?;
            registered_functions.shrink_to_fit();
            (vm_proto, registered_functions)
        };

        // In the runtime environment, Wasm blobs must export a global symbol named
        // `__heap_base` indicating where the memory allocator is allowed to allocate memory.
        let heap_base = vm_proto
            .global_value("__heap_base")
            .map_err(|_| NewErr::HeapBaseNotFound)?;

        Ok(ExternalsVmPrototype {
            vm_proto,
            heap_base,
            registered_functions,
        })
    }

    /// Starts the VM, calling the function passed as parameter.
    pub fn run(self, function_to_call: &str, data: &[u8]) -> Result<ReadyToRun, NewErr> {
        self.run_vectored(function_to_call, iter::once(data))
    }

    /// Same as [`ExternalsVmPrototype::run`], except that the function desn't need any parameter.
    pub fn run_no_param(self, function_to_call: &str) -> Result<ReadyToRun, NewErr> {
        self.run_vectored(function_to_call, iter::empty::<Vec<u8>>())
    }

    /// Same as [`ExternalsVmPrototype::run`], except that the function parameter can be passed as
    /// a list of buffers. All the buffers will be concatenated in memory.
    pub fn run_vectored(
        self,
        function_to_call: &str,
        data: impl Iterator<Item = impl AsRef<[u8]>> + Clone,
    ) -> Result<ReadyToRun, NewErr> {
        let mut data_len_u32: u32 = 0;
        for data in data.clone() {
            let len = u32::try_from(data.as_ref().len()).map_err(|_| NewErr::DataSizeOverflow)?;
            data_len_u32 = data_len_u32
                .checked_add(len)
                .ok_or(NewErr::DataSizeOverflow)?;
        }

        // Now create the actual virtual machine. We pass as parameter `heap_base` as the location
        // of the input data.
        let mut vm = self.vm_proto.start(
            function_to_call,
            &[
                vm::WasmValue::I32(i32::from_ne_bytes(self.heap_base.to_ne_bytes())),
                vm::WasmValue::I32(i32::from_ne_bytes(data_len_u32.to_ne_bytes())),
            ],
        )?;

        // Now writing the input data into the VM.
        let mut after_input_data = self.heap_base;
        for data in data {
            let data = data.as_ref();
            vm.write_memory(after_input_data, data).unwrap();
            after_input_data = after_input_data
                .checked_add(u32::try_from(data.len()).unwrap())
                .unwrap();
        }

        // Initialize the state of the memory allocator. This is the allocator that is later used
        // when the Wasm code requests variable-length data.
        let allocator = allocator::FreeingBumpHeapAllocator::new(after_input_data);

        Ok(ReadyToRun {
            resume_value: None,
            inner: Inner {
                vm,
                heap_base: self.heap_base,
                registered_functions: self.registered_functions,
                within_storage_transaction: false,
                allocator,
            },
        })
    }
}

/// Running virtual machine.
#[must_use]
#[derive(derive_more::From)]
pub enum ExternalsVm {
    /// Wasm virtual machine is ready to be run. Call [`ReadyToRun::run`] to make progress.
    #[from]
    ReadyToRun(ReadyToRun),
    /// Function execution has succeeded. Contains the return value of the call.
    #[from]
    Finished(Finished),
    /// The Wasm blob did something that doesn't conform to the runtime environment.
    Error {
        /// Virtual machine ready to be used again.
        prototype: ExternalsVmPrototype,
        /// Error that happened.
        error: Error,
    },
    /// Must load an storage value.
    #[from]
    ExternalStorageGet(ExternalStorageGet),
    /// Must set an storage value.
    #[from]
    ExternalStorageSet(ExternalStorageSet),
    /// See documentation of [`ExternalStorageAppend`].
    #[from]
    ExternalStorageAppend(ExternalStorageAppend),
    /// Must remove all the storage values starting with a certain prefix.
    #[from]
    ExternalStorageClearPrefix(ExternalStorageClearPrefix),
    /// Need to provide the trie root of the storage.
    #[from]
    ExternalStorageRoot(ExternalStorageRoot),
    /// Need to provide the trie root of the changes trie.
    #[from]
    ExternalStorageChangesRoot(ExternalStorageChangesRoot),
    /// Need to provide the storage key that follows a specific one.
    #[from]
    ExternalStorageNextKey(ExternalStorageNextKey),
    /// Must the set value of an offchain storage entry.
    #[from]
    ExternalOffchainStorageSet(ExternalOffchainStorageSet),
    /// Need to call `Core_version` on the given Wasm code and return the raw output (i.e.
    /// still SCALE-encoded), or an error if the call has failed.
    #[from]
    CallRuntimeVersion(CallRuntimeVersion),
    /// Declares the start of a storage transaction. See [`ExternalsVm::EndStorageTransaction`].
    ///
    /// Guaranteed by the code in this module to never happen while already within a transaction.
    /// If the runtime attempts to start a nested transaction, an [`ExternalsVm::Error`] is
    /// generated instead.
    #[from]
    StartStorageTransaction(StartStorageTransaction),
    /// Ends a storage transaction. All changes made to the storage (e.g. through a
    /// [`ExternalsVm::ExternalStorageSet`]) since the previous
    /// [`ExternalsVm::StartStorageTransaction`] must be rolled back if `rollback` is true.
    ///
    /// Guaranteed by the code in this module to never happen if no transaction is in progress.
    /// If the runtime attempts to end a non-existing transaction, an [`ExternalsVm::Error`] is
    /// generated instead.
    EndStorageTransaction {
        /// Object used to resume execution.
        resume: EndStorageTransaction,
        /// If true, changes must be rolled back.
        #[must_use]
        rollback: bool,
    },
    /// Runtime has emitted a log entry.
    #[from]
    LogEmit(LogEmit),
}

/// Virtual machine is ready to run.
pub struct ReadyToRun {
    inner: Inner,
    resume_value: Option<vm::WasmValue>,
}

impl ReadyToRun {
    /// Runs the virtual machine until something important happens.
    ///
    /// > **Note**: This is when the actual CPU-heavy computation happens.
    pub fn run(mut self) -> ExternalsVm {
        loop {
            // `vm::ExecOutcome::Interrupted` is by far the variant that requires the most
            // handling code. As such, special-case all other variants before.
            let (id, params) = match self.inner.vm.run(self.resume_value) {
                Ok(vm::ExecOutcome::Interrupted { id, params }) => (id, params),

                Ok(vm::ExecOutcome::Finished {
                    return_value: Ok(Some(vm::WasmValue::I64(ret))),
                }) => {
                    // Wasm virtual machine has successfully returned.

                    if self.inner.within_storage_transaction {
                        return ExternalsVm::Error {
                            prototype: self.inner.into_prototype(),
                            error: Error::FinishedWithPendingTransaction,
                        };
                    }

                    // Turn the `i64` into a `u64`, not changing any bit.
                    let ret = u64::from_ne_bytes(ret.to_ne_bytes());

                    // According to the runtime environment specifications, the return value is two
                    // consecutive I32s representing the length and size of the SCALE-encoded
                    // return value.
                    let ret_len = u32::try_from(ret >> 32).unwrap();
                    let ret_ptr = u32::try_from(ret & 0xffffffff).unwrap();

                    let ret_data = self
                        .inner
                        .vm
                        .read_memory(ret_ptr, ret_len)
                        .map(|d| d.as_ref().to_vec());
                    if let Ok(value) = ret_data {
                        return ExternalsVm::Finished(Finished {
                            inner: self.inner,
                            value,
                        });
                    } else {
                        let error = Error::ReturnedPtrOutOfRange {
                            pointer: ret_ptr,
                            size: ret_len,
                            memory_size: self.inner.vm.memory_size(),
                        };

                        return ExternalsVm::Error {
                            prototype: self.inner.into_prototype(),
                            error,
                        };
                    }
                }

                Ok(vm::ExecOutcome::Finished {
                    return_value: Ok(return_value),
                }) => {
                    // The Wasm function has successfully returned, but the specs require that it
                    // returns a `i64`.
                    return ExternalsVm::Error {
                        prototype: self.inner.into_prototype(),
                        error: Error::BadReturnValue {
                            actual: return_value.map(|v| v.ty()),
                        },
                    };
                }

                Ok(vm::ExecOutcome::Finished {
                    return_value: Err(()),
                }) => {
                    return ExternalsVm::Error {
                        error: Error::Trapped,
                        prototype: self.inner.into_prototype(),
                    }
                }

                Err(vm::RunErr::BadValueTy { .. }) => {
                    // Tried to inject back the value returned by an externality, but it doesn't
                    // match what the Wasm code expects.
                    // TODO: check signatures at initialization instead?
                    return ExternalsVm::Error {
                        prototype: self.inner.into_prototype(),
                        error: Error::ReturnValueTypeMismatch,
                    };
                }

                Err(vm::RunErr::Poisoned) => {
                    // Can only happen if there's a bug somewhere.
                    unreachable!()
                }
            };

            // The Wasm code has called an externality. The `id` is a value that we passed
            // at initialization, and corresponds to an index in `registered_functions`.
            let externality = *self.inner.registered_functions.get_mut(id).unwrap();

            // Check that the actual number of parameters matches the expected number.
            // This is done ahead of time in order to not forget.
            let expected_params_num = match externality {
                Externality::ext_storage_set_version_1 => 2,
                Externality::ext_storage_get_version_1 => 1,
                Externality::ext_storage_read_version_1 => 3,
                Externality::ext_storage_clear_version_1 => 1,
                Externality::ext_storage_exists_version_1 => 1,
                Externality::ext_storage_clear_prefix_version_1 => 1,
                Externality::ext_storage_root_version_1 => 0,
                Externality::ext_storage_changes_root_version_1 => 1,
                Externality::ext_storage_next_key_version_1 => 1,
                Externality::ext_storage_append_version_1 => 2,
                Externality::ext_storage_child_set_version_1 => todo!(),
                Externality::ext_storage_child_get_version_1 => todo!(),
                Externality::ext_storage_child_read_version_1 => todo!(),
                Externality::ext_storage_child_clear_version_1 => todo!(),
                Externality::ext_storage_child_storage_kill_version_1 => todo!(),
                Externality::ext_storage_child_exists_version_1 => todo!(),
                Externality::ext_storage_child_clear_prefix_version_1 => todo!(),
                Externality::ext_storage_child_root_version_1 => todo!(),
                Externality::ext_storage_child_next_key_version_1 => todo!(),
                Externality::ext_storage_start_transaction_version_1 => 0,
                Externality::ext_storage_rollback_transaction_version_1 => 0,
                Externality::ext_storage_commit_transaction_version_1 => 0,
                Externality::ext_default_child_storage_get_version_1 => todo!(),
                Externality::ext_default_child_storage_storage_kill_version_1 => todo!(),
                Externality::ext_default_child_storage_set_version_1 => todo!(),
                Externality::ext_default_child_storage_clear_version_1 => todo!(),
                Externality::ext_default_child_storage_root_version_1 => todo!(),
                Externality::ext_crypto_ed25519_public_keys_version_1 => todo!(),
                Externality::ext_crypto_ed25519_generate_version_1 => todo!(),
                Externality::ext_crypto_ed25519_sign_version_1 => todo!(),
                Externality::ext_crypto_ed25519_verify_version_1 => 3,
                Externality::ext_crypto_sr25519_public_keys_version_1 => todo!(),
                Externality::ext_crypto_sr25519_generate_version_1 => todo!(),
                Externality::ext_crypto_sr25519_sign_version_1 => todo!(),
                Externality::ext_crypto_sr25519_verify_version_1 => 3,
                Externality::ext_crypto_sr25519_verify_version_2 => 3,
                Externality::ext_crypto_secp256k1_ecdsa_recover_version_1 => 2,
                Externality::ext_crypto_secp256k1_ecdsa_recover_compressed_version_1 => 2,
                Externality::ext_crypto_start_batch_verify_version_1 => 0,
                Externality::ext_crypto_finish_batch_verify_version_1 => 0,
                Externality::ext_hashing_keccak_256_version_1 => 1,
                Externality::ext_hashing_sha2_256_version_1 => todo!(),
                Externality::ext_hashing_blake2_128_version_1 => 1,
                Externality::ext_hashing_blake2_256_version_1 => 1,
                Externality::ext_hashing_twox_64_version_1 => 1,
                Externality::ext_hashing_twox_128_version_1 => 1,
                Externality::ext_hashing_twox_256_version_1 => 1,
                Externality::ext_offchain_index_set_version_1 => 2,
                Externality::ext_offchain_index_clear_version_1 => 1,
                Externality::ext_offchain_is_validator_version_1 => todo!(),
                Externality::ext_offchain_submit_transaction_version_1 => todo!(),
                Externality::ext_offchain_network_state_version_1 => todo!(),
                Externality::ext_offchain_timestamp_version_1 => todo!(),
                Externality::ext_offchain_sleep_until_version_1 => todo!(),
                Externality::ext_offchain_random_seed_version_1 => todo!(),
                Externality::ext_offchain_local_storage_set_version_1 => todo!(),
                Externality::ext_offchain_local_storage_compare_and_set_version_1 => todo!(),
                Externality::ext_offchain_local_storage_get_version_1 => todo!(),
                Externality::ext_offchain_http_request_start_version_1 => todo!(),
                Externality::ext_offchain_http_request_add_header_version_1 => todo!(),
                Externality::ext_offchain_http_request_write_body_version_1 => todo!(),
                Externality::ext_offchain_http_response_wait_version_1 => todo!(),
                Externality::ext_offchain_http_response_headers_version_1 => todo!(),
                Externality::ext_offchain_http_response_read_body_version_1 => todo!(),
                Externality::ext_sandbox_instantiate_version_1 => todo!(),
                Externality::ext_sandbox_invoke_version_1 => todo!(),
                Externality::ext_sandbox_memory_new_version_1 => todo!(),
                Externality::ext_sandbox_memory_get_version_1 => todo!(),
                Externality::ext_sandbox_memory_set_version_1 => todo!(),
                Externality::ext_sandbox_memory_teardown_version_1 => todo!(),
                Externality::ext_sandbox_instance_teardown_version_1 => todo!(),
                Externality::ext_sandbox_get_global_val_version_1 => todo!(),
                Externality::ext_trie_blake2_256_root_version_1 => 1,
                Externality::ext_trie_blake2_256_ordered_root_version_1 => 1,
                Externality::ext_misc_chain_id_version_1 => 0,
                Externality::ext_misc_print_num_version_1 => 1,
                Externality::ext_misc_print_utf8_version_1 => 1,
                Externality::ext_misc_print_hex_version_1 => 1,
                Externality::ext_misc_runtime_version_version_1 => 1,
                Externality::ext_allocator_malloc_version_1 => 1,
                Externality::ext_allocator_free_version_1 => 1,
                Externality::ext_logging_log_version_1 => 3,
            };
            if params.len() != expected_params_num {
                return ExternalsVm::Error {
                    error: Error::ParamsCountMismatch {
                        function: externality.name(),
                        expected: expected_params_num,
                        actual: params.len(),
                    },
                    prototype: self.inner.into_prototype(),
                };
            }

            macro_rules! expect_pointer_size {
                ($num:expr) => {{
                    let val = match &params[$num] {
                        vm::WasmValue::I64(v) => u64::from_ne_bytes(v.to_ne_bytes()),
                        v => {
                            return ExternalsVm::Error {
                                error: Error::WrongParamTy {
                                    function: externality.name(),
                                    param_num: $num,
                                    expected: vm::ValueType::I64,
                                    actual: v.ty(),
                                },
                                prototype: self.inner.into_prototype(),
                            }
                        },
                    };

                    let len = u32::try_from(val >> 32).unwrap();
                    let ptr = u32::try_from(val & 0xffffffff).unwrap();

                    match self.inner.vm.read_memory(ptr, len).map(|v| v.as_ref().to_vec()) { // TODO: no; keep the impl AsRef<[u8]>; however Rust doesn't like the way we borrow things
                        Ok(v) => v,
                        Err(()) => {
                            return ExternalsVm::Error {
                                error: Error::ParamOutOfRange {
                                    function: externality.name(),
                                    param_num: $num,
                                    pointer: ptr,
                                    length: len,
                                },
                                prototype: self.inner.into_prototype(),
                            }
                        }
                    }
                }}
            }

            macro_rules! expect_pointer_size_raw {
                ($num:expr) => {{
                    let val = match &params[$num] {
                        vm::WasmValue::I64(v) => u64::from_ne_bytes(v.to_ne_bytes()),
                        v => {
                            return ExternalsVm::Error {
                                error: Error::WrongParamTy {
                                    function: externality.name(),
                                    param_num: $num,
                                    expected: vm::ValueType::I64,
                                    actual: v.ty(),
                                },
                                prototype: self.inner.into_prototype(),
                            };
                        }
                    };

                    let len = u32::try_from(val >> 32).unwrap();
                    let ptr = u32::try_from(val & 0xffffffff).unwrap();
                    (ptr, len)
                }};
            }

            macro_rules! expect_pointer_constant_size {
                ($num:expr, $size:expr) => {{
                    let ptr = match params[$num] {
                        vm::WasmValue::I32(v) => u32::from_ne_bytes(v.to_ne_bytes()),
                        v => {
                            return ExternalsVm::Error {
                                error: Error::WrongParamTy {
                                    function: externality.name(),
                                    param_num: $num,
                                    expected: vm::ValueType::I32,
                                    actual: v.ty(),
                                },
                                prototype: self.inner.into_prototype(),
                            }
                        },
                    };

                    match self.inner.vm.read_memory(ptr, $size).map(|v| v.as_ref().to_vec()) { // TODO: no; keep the impl AsRef<[u8]>; however Rust doesn't like the way we borrow things
                        Ok(v) => v,
                        Err(()) => {
                            return ExternalsVm::Error {
                                error: Error::ParamOutOfRange {
                                    function: externality.name(),
                                    param_num: $num,
                                    pointer: ptr,
                                    length: $size,
                                },
                                prototype: self.inner.into_prototype(),
                            }
                        }
                    }
                }}
            }

            macro_rules! expect_u32 {
                ($num:expr) => {{
                    match &params[$num] {
                        vm::WasmValue::I32(v) => u32::from_ne_bytes(v.to_ne_bytes()),
                        v => {
                            return ExternalsVm::Error {
                                error: Error::WrongParamTy {
                                    function: externality.name(),
                                    param_num: $num,
                                    expected: vm::ValueType::I32,
                                    actual: v.ty(),
                                },
                                prototype: self.inner.into_prototype(),
                            }
                        }
                    }
                }};
            }

            // Handle the function calls.
            // Some of these enum variants simply change the state of `self`, while most of them
            // instead return an `ExternalVm` to the user.
            match externality {
                Externality::ext_storage_set_version_1 => {
                    let key = expect_pointer_size!(0);
                    let value = expect_pointer_size!(1);
                    return ExternalsVm::ExternalStorageSet(ExternalStorageSet {
                        key,
                        value: Some(value),
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_get_version_1 => {
                    let key = expect_pointer_size!(0);
                    return ExternalsVm::ExternalStorageGet(ExternalStorageGet {
                        key,
                        calling: id,
                        value_out_ptr: None,
                        offset: 0,
                        max_size: u32::max_value(),
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_read_version_1 => {
                    let key = expect_pointer_size!(0);
                    let (value_out_ptr, value_out_size) = expect_pointer_size_raw!(1);
                    let offset = expect_u32!(2);
                    return ExternalsVm::ExternalStorageGet(ExternalStorageGet {
                        key,
                        calling: id,
                        value_out_ptr: Some(value_out_ptr),
                        offset,
                        max_size: value_out_size,
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_clear_version_1 => {
                    let key = expect_pointer_size!(0);
                    return ExternalsVm::ExternalStorageSet(ExternalStorageSet {
                        key,
                        value: None,
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_exists_version_1 => {
                    let key = expect_pointer_size!(0);
                    return ExternalsVm::ExternalStorageGet(ExternalStorageGet {
                        key,
                        calling: id,
                        value_out_ptr: None,
                        offset: 0,
                        max_size: 0,
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_clear_prefix_version_1 => {
                    let prefix = expect_pointer_size!(0);
                    return ExternalsVm::ExternalStorageClearPrefix(ExternalStorageClearPrefix {
                        prefix,
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_root_version_1 => {
                    return ExternalsVm::ExternalStorageRoot(ExternalStorageRoot {
                        inner: self.inner,
                    })
                }
                Externality::ext_storage_changes_root_version_1 => {
                    // TODO: there's a parameter
                    return ExternalsVm::ExternalStorageChangesRoot(ExternalStorageChangesRoot {
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_next_key_version_1 => {
                    let key = expect_pointer_size!(0);
                    return ExternalsVm::ExternalStorageNextKey(ExternalStorageNextKey {
                        key,
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_append_version_1 => {
                    let key = expect_pointer_size!(0);
                    let value = expect_pointer_size!(1);
                    return ExternalsVm::ExternalStorageAppend(ExternalStorageAppend {
                        key,
                        value,
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_child_set_version_1 => todo!(),
                Externality::ext_storage_child_get_version_1 => todo!(),
                Externality::ext_storage_child_read_version_1 => todo!(),
                Externality::ext_storage_child_clear_version_1 => todo!(),
                Externality::ext_storage_child_storage_kill_version_1 => todo!(),
                Externality::ext_storage_child_exists_version_1 => todo!(),
                Externality::ext_storage_child_clear_prefix_version_1 => todo!(),
                Externality::ext_storage_child_root_version_1 => todo!(),
                Externality::ext_storage_child_next_key_version_1 => todo!(),
                Externality::ext_storage_start_transaction_version_1 => {
                    if self.inner.within_storage_transaction {
                        return ExternalsVm::Error {
                            error: Error::NestedTransaction,
                            prototype: self.inner.into_prototype(),
                        };
                    }

                    self.inner.within_storage_transaction = true;
                    return ExternalsVm::StartStorageTransaction(StartStorageTransaction {
                        inner: self.inner,
                    });
                }
                Externality::ext_storage_rollback_transaction_version_1 => {
                    if !self.inner.within_storage_transaction {
                        return ExternalsVm::Error {
                            error: Error::NoActiveTransaction,
                            prototype: self.inner.into_prototype(),
                        };
                    }

                    self.inner.within_storage_transaction = false;
                    return ExternalsVm::EndStorageTransaction {
                        resume: EndStorageTransaction { inner: self.inner },
                        rollback: true,
                    };
                }
                Externality::ext_storage_commit_transaction_version_1 => {
                    if !self.inner.within_storage_transaction {
                        return ExternalsVm::Error {
                            error: Error::NoActiveTransaction,
                            prototype: self.inner.into_prototype(),
                        };
                    }

                    self.inner.within_storage_transaction = false;
                    return ExternalsVm::EndStorageTransaction {
                        resume: EndStorageTransaction { inner: self.inner },
                        rollback: false,
                    };
                }
                Externality::ext_default_child_storage_get_version_1 => todo!(),
                Externality::ext_default_child_storage_storage_kill_version_1 => todo!(),
                Externality::ext_default_child_storage_set_version_1 => todo!(),
                Externality::ext_default_child_storage_clear_version_1 => todo!(),
                Externality::ext_default_child_storage_root_version_1 => todo!(),
                Externality::ext_crypto_ed25519_public_keys_version_1 => todo!(),
                Externality::ext_crypto_ed25519_generate_version_1 => todo!(),
                Externality::ext_crypto_ed25519_sign_version_1 => todo!(),
                Externality::ext_crypto_ed25519_verify_version_1 => {
                    let sig = expect_pointer_constant_size!(0, 64);
                    let message = expect_pointer_size!(1);
                    let pubkey = expect_pointer_constant_size!(2, 32);

                    // TODO: copy overhead?
                    let success = if let Ok(public_key) =
                        ed25519_dalek::PublicKey::from_bytes(&pubkey)
                    {
                        // TODO: copy overhead?
                        let signature =
                            ed25519_dalek::Signature::new(<[u8; 64]>::try_from(&sig[..]).unwrap());
                        public_key.verify_strict(&message, &signature).is_ok()
                    } else {
                        false
                    };

                    self = ReadyToRun {
                        resume_value: Some(vm::WasmValue::I32(if success { 1 } else { 0 })),
                        inner: self.inner,
                    };
                }
                Externality::ext_crypto_sr25519_public_keys_version_1 => todo!(),
                Externality::ext_crypto_sr25519_generate_version_1 => todo!(),
                Externality::ext_crypto_sr25519_sign_version_1 => todo!(),
                Externality::ext_crypto_sr25519_verify_version_1 => {
                    let sig = expect_pointer_constant_size!(0, 64);
                    let message = expect_pointer_size!(1);
                    let pubkey = expect_pointer_constant_size!(2, 32);

                    // The `unwrap()` below can only panic if the input is the wrong length, which
                    // we know can't happen.
                    // TODO: copy overhead?
                    let signing_public_key = schnorrkel::PublicKey::from_bytes(&pubkey).unwrap();
                    let success = signing_public_key
                        .verify_simple_preaudit_deprecated(b"substrate", &message, &sig)
                        .is_ok();

                    self = ReadyToRun {
                        resume_value: Some(vm::WasmValue::I32(if success { 1 } else { 0 })),
                        inner: self.inner,
                    };
                }
                Externality::ext_crypto_sr25519_verify_version_2 => {
                    let sig = expect_pointer_constant_size!(0, 64);
                    let message = expect_pointer_size!(1);
                    let pubkey = expect_pointer_constant_size!(2, 32);

                    // The two `unwrap()`s below can only panic if the input is the wrong length,
                    // which we know can't happen.
                    // TODO: copy overhead?
                    let signing_public_key = schnorrkel::PublicKey::from_bytes(&pubkey).unwrap();
                    // TODO: copy overhead?
                    let signature = schnorrkel::Signature::from_bytes(&sig).unwrap();

                    let success = signing_public_key
                        .verify_simple(b"substrate", &message, &signature)
                        .is_ok();

                    self = ReadyToRun {
                        resume_value: Some(vm::WasmValue::I32(if success { 1 } else { 0 })),
                        inner: self.inner,
                    };
                }
                Externality::ext_crypto_secp256k1_ecdsa_recover_version_1 => {
                    // TODO: clean up
                    #[derive(parity_scale_codec::Encode)]
                    enum EcdsaVerifyError {
                        RSError,
                        VError,
                        BadSignature,
                    }

                    let sig = expect_pointer_constant_size!(0, 65);
                    let msg = expect_pointer_constant_size!(1, 32);

                    let result = (|| -> Result<_, EcdsaVerifyError> {
                        let rs = secp256k1::Signature::parse_slice(&sig[0..64])
                            .map_err(|_| EcdsaVerifyError::RSError)?;
                        let v = secp256k1::RecoveryId::parse(if sig[64] > 26 {
                            sig[64] - 27
                        } else {
                            sig[64]
                        } as u8)
                        .map_err(|_| EcdsaVerifyError::VError)?;
                        let pubkey = secp256k1::recover(
                            &secp256k1::Message::parse_slice(&msg).unwrap(),
                            &rs,
                            &v,
                        )
                        .map_err(|_| EcdsaVerifyError::BadSignature)?;
                        let mut res = [0u8; 64];
                        res.copy_from_slice(&pubkey.serialize()[1..65]);
                        Ok(res)
                    })();
                    let result_encoded = parity_scale_codec::Encode::encode(&result);

                    match self.inner.alloc_write_and_return_pointer_size(
                        externality.name(),
                        iter::once(&result_encoded),
                    ) {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_crypto_secp256k1_ecdsa_recover_compressed_version_1 => {
                    // TODO: clean up
                    #[derive(parity_scale_codec::Encode)]
                    enum EcdsaVerifyError {
                        RSError,
                        VError,
                        BadSignature,
                    }

                    let sig = expect_pointer_constant_size!(0, 65);
                    let msg = expect_pointer_constant_size!(1, 32);

                    let result = (|| -> Result<_, EcdsaVerifyError> {
                        let rs = secp256k1::Signature::parse_slice(&sig[0..64])
                            .map_err(|_| EcdsaVerifyError::RSError)?;
                        let v = secp256k1::RecoveryId::parse(if sig[64] > 26 {
                            sig[64] - 27
                        } else {
                            sig[64]
                        } as u8)
                        .map_err(|_| EcdsaVerifyError::VError)?;
                        let pubkey = secp256k1::recover(
                            &secp256k1::Message::parse_slice(&msg).unwrap(),
                            &rs,
                            &v,
                        )
                        .map_err(|_| EcdsaVerifyError::BadSignature)?;
                        Ok(pubkey.serialize_compressed())
                    })();
                    let result_encoded = parity_scale_codec::Encode::encode(&result);

                    match self.inner.alloc_write_and_return_pointer_size(
                        externality.name(),
                        iter::once(&result_encoded),
                    ) {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_crypto_start_batch_verify_version_1 => {
                    self = ReadyToRun {
                        resume_value: None,
                        inner: self.inner,
                    };
                }
                Externality::ext_crypto_finish_batch_verify_version_1 => {
                    self = ReadyToRun {
                        // TODO: wrong! this is a dummy implementation meaning that all
                        // signature verifications are always successful
                        resume_value: Some(vm::WasmValue::I32(1)),
                        inner: self.inner,
                    };
                }
                Externality::ext_hashing_keccak_256_version_1 => {
                    let data = expect_pointer_size!(0);

                    let mut keccak = tiny_keccak::Keccak::v256();
                    keccak.update(&data);
                    let mut out = [0u8; 32];
                    keccak.finalize(&mut out);

                    match self
                        .inner
                        .alloc_write_and_return_pointer(externality.name(), iter::once(&out))
                    {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_hashing_sha2_256_version_1 => {
                    let data = expect_pointer_size!(0);

                    let mut hasher = sha2::Sha256::new();
                    hasher.update(data);

                    match self.inner.alloc_write_and_return_pointer(
                        externality.name(),
                        iter::once(hasher.finalize().as_slice()),
                    ) {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_hashing_blake2_128_version_1 => {
                    let data = expect_pointer_size!(0);
                    let out = blake2_rfc::blake2b::blake2b(16, &[], &data);

                    match self.inner.alloc_write_and_return_pointer(
                        externality.name(),
                        iter::once(out.as_bytes()),
                    ) {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_hashing_blake2_256_version_1 => {
                    let data = expect_pointer_size!(0);
                    let out = blake2_rfc::blake2b::blake2b(32, &[], &data);

                    match self.inner.alloc_write_and_return_pointer(
                        externality.name(),
                        iter::once(out.as_bytes()),
                    ) {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_hashing_twox_64_version_1 => {
                    let data = expect_pointer_size!(0);

                    let mut h0 = twox_hash::XxHash::with_seed(0);
                    h0.write(&data);
                    let r0 = h0.finish();

                    match self.inner.alloc_write_and_return_pointer(
                        externality.name(),
                        iter::once(&r0.to_le_bytes()),
                    ) {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_hashing_twox_128_version_1 => {
                    let data = expect_pointer_size!(0);

                    let mut h0 = twox_hash::XxHash::with_seed(0);
                    let mut h1 = twox_hash::XxHash::with_seed(1);
                    h0.write(&data);
                    h1.write(&data);
                    let r0 = h0.finish();
                    let r1 = h1.finish();

                    match self.inner.alloc_write_and_return_pointer(
                        externality.name(),
                        iter::once(&r0.to_le_bytes()).chain(iter::once(&r1.to_le_bytes())),
                    ) {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_hashing_twox_256_version_1 => {
                    let data = expect_pointer_size!(0);

                    let mut h0 = twox_hash::XxHash::with_seed(0);
                    let mut h1 = twox_hash::XxHash::with_seed(1);
                    let mut h2 = twox_hash::XxHash::with_seed(2);
                    let mut h3 = twox_hash::XxHash::with_seed(3);
                    h0.write(&data);
                    h1.write(&data);
                    h2.write(&data);
                    h3.write(&data);
                    let r0 = h0.finish();
                    let r1 = h1.finish();
                    let r2 = h2.finish();
                    let r3 = h3.finish();

                    match self.inner.alloc_write_and_return_pointer(
                        externality.name(),
                        iter::once(&r0.to_le_bytes())
                            .chain(iter::once(&r1.to_le_bytes()))
                            .chain(iter::once(&r2.to_le_bytes()))
                            .chain(iter::once(&r3.to_le_bytes())),
                    ) {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_offchain_index_set_version_1 => {
                    let key = expect_pointer_size!(0);
                    let value = expect_pointer_size!(1);
                    return ExternalsVm::ExternalOffchainStorageSet(ExternalOffchainStorageSet {
                        key,
                        value: Some(value),
                        inner: self.inner,
                    });
                }
                Externality::ext_offchain_index_clear_version_1 => {
                    let key = expect_pointer_size!(0);
                    return ExternalsVm::ExternalOffchainStorageSet(ExternalOffchainStorageSet {
                        key,
                        value: None,
                        inner: self.inner,
                    });
                }
                Externality::ext_offchain_is_validator_version_1 => todo!(),
                Externality::ext_offchain_submit_transaction_version_1 => todo!(),
                Externality::ext_offchain_network_state_version_1 => todo!(),
                Externality::ext_offchain_timestamp_version_1 => todo!(),
                Externality::ext_offchain_sleep_until_version_1 => todo!(),
                Externality::ext_offchain_random_seed_version_1 => todo!(),
                Externality::ext_offchain_local_storage_set_version_1 => todo!(),
                Externality::ext_offchain_local_storage_compare_and_set_version_1 => todo!(),
                Externality::ext_offchain_local_storage_get_version_1 => todo!(),
                Externality::ext_offchain_http_request_start_version_1 => todo!(),
                Externality::ext_offchain_http_request_add_header_version_1 => todo!(),
                Externality::ext_offchain_http_request_write_body_version_1 => todo!(),
                Externality::ext_offchain_http_response_wait_version_1 => todo!(),
                Externality::ext_offchain_http_response_headers_version_1 => todo!(),
                Externality::ext_offchain_http_response_read_body_version_1 => todo!(),
                Externality::ext_sandbox_instantiate_version_1 => todo!(),
                Externality::ext_sandbox_invoke_version_1 => todo!(),
                Externality::ext_sandbox_memory_new_version_1 => todo!(),
                Externality::ext_sandbox_memory_get_version_1 => todo!(),
                Externality::ext_sandbox_memory_set_version_1 => todo!(),
                Externality::ext_sandbox_memory_teardown_version_1 => todo!(),
                Externality::ext_sandbox_instance_teardown_version_1 => todo!(),
                Externality::ext_sandbox_get_global_val_version_1 => todo!(),
                Externality::ext_trie_blake2_256_root_version_1 => {
                    let encoded = expect_pointer_size!(0);

                    let elements = match Vec::<(Vec<u8>, Vec<u8>)>::decode_all(&encoded) {
                        Ok(e) => e,
                        Err(err) => {
                            return ExternalsVm::Error {
                                error: Error::ParamDecodeError(err),
                                prototype: self.inner.into_prototype(),
                            }
                        }
                    };

                    // TODO: optimize this
                    let mut trie = crate::trie::Trie::new();
                    for (key, value) in elements {
                        trie.insert(&key, value);
                    }
                    let out = trie.root_merkle_value(None);

                    match self
                        .inner
                        .alloc_write_and_return_pointer(externality.name(), iter::once(&out))
                    {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_trie_blake2_256_ordered_root_version_1 => {
                    let encoded = expect_pointer_size!(0);

                    let elements = match Vec::<Vec<u8>>::decode_all(&encoded) {
                        Ok(e) => e,
                        Err(err) => {
                            return ExternalsVm::Error {
                                error: Error::ParamDecodeError(err),
                                prototype: self.inner.into_prototype(),
                            }
                        }
                    };

                    // TODO: optimize this
                    let mut trie = crate::trie::Trie::new();
                    for (idx, value) in elements.into_iter().enumerate() {
                        let key = util::encode_scale_compact_usize(idx);
                        trie.insert(key.as_ref(), value);
                    }
                    let out = trie.root_merkle_value(None);

                    match self
                        .inner
                        .alloc_write_and_return_pointer(externality.name(), iter::once(&out))
                    {
                        ExternalsVm::ReadyToRun(r) => self = r,
                        other => return other,
                    }
                }
                Externality::ext_misc_chain_id_version_1 => {
                    // TODO: this parachain-related function always returns 42 at the moment
                    self = ReadyToRun {
                        resume_value: Some(vm::WasmValue::I32(42)),
                        inner: self.inner,
                    };
                }
                Externality::ext_misc_print_num_version_1 => {
                    let num = match params[0] {
                        vm::WasmValue::I64(v) => u64::from_ne_bytes(v.to_ne_bytes()),
                        v => {
                            return ExternalsVm::Error {
                                error: Error::WrongParamTy {
                                    function: externality.name(),
                                    param_num: 0,
                                    expected: vm::ValueType::I64,
                                    actual: v.ty(),
                                },
                                prototype: self.inner.into_prototype(),
                            }
                        }
                    };

                    let log_entry = format!("{}", num);
                    return ExternalsVm::LogEmit(LogEmit {
                        inner: self.inner,
                        log_entry,
                    });
                }
                Externality::ext_misc_print_utf8_version_1 => {
                    let data = expect_pointer_size!(0);
                    let log_entry = match String::from_utf8(data) {
                        Ok(m) => m,
                        Err(error) => {
                            return ExternalsVm::Error {
                                error: Error::Utf8Error {
                                    function: externality.name(),
                                    param_num: 2,
                                    error: error.utf8_error(),
                                },
                                prototype: self.inner.into_prototype(),
                            };
                        }
                    };

                    return ExternalsVm::LogEmit(LogEmit {
                        inner: self.inner,
                        log_entry,
                    });
                }
                Externality::ext_misc_print_hex_version_1 => {
                    let data = expect_pointer_size!(0);
                    let log_entry = hex::encode(&data);
                    return ExternalsVm::LogEmit(LogEmit {
                        inner: self.inner,
                        log_entry,
                    });
                }
                Externality::ext_misc_runtime_version_version_1 => {
                    let wasm_blob = expect_pointer_size!(0);
                    return ExternalsVm::CallRuntimeVersion(CallRuntimeVersion {
                        inner: self.inner,
                        wasm_blob,
                    });
                }
                Externality::ext_allocator_malloc_version_1 => {
                    let size = expect_u32!(0);

                    let ptr = match self
                        .inner
                        .allocator
                        .allocate(&mut MemAccess(&mut self.inner.vm), size)
                    {
                        Ok(p) => p,
                        Err(_) => {
                            return ExternalsVm::Error {
                                error: Error::OutOfMemory {
                                    function: externality.name(),
                                    requested_size: size,
                                },
                                prototype: self.inner.into_prototype(),
                            }
                        }
                    };

                    let ptr_i32 = i32::from_ne_bytes(ptr.to_ne_bytes());
                    self = ReadyToRun {
                        resume_value: Some(vm::WasmValue::I32(ptr_i32)),
                        inner: self.inner,
                    };
                }
                Externality::ext_allocator_free_version_1 => {
                    let pointer = expect_u32!(0);
                    match self
                        .inner
                        .allocator
                        .deallocate(&mut MemAccess(&mut self.inner.vm), pointer)
                    {
                        Ok(()) => {}
                        Err(_) => {
                            return ExternalsVm::Error {
                                error: Error::FreeError { pointer },
                                prototype: self.inner.into_prototype(),
                            }
                        }
                    };

                    self = ReadyToRun {
                        resume_value: None,
                        inner: self.inner,
                    };
                }
                Externality::ext_logging_log_version_1 => {
                    let _log_level = expect_u32!(0);
                    let _target = expect_pointer_size!(1);
                    let message = expect_pointer_size!(2);
                    let log_entry = match String::from_utf8(message) {
                        Ok(m) => m,
                        Err(error) => {
                            return ExternalsVm::Error {
                                error: Error::Utf8Error {
                                    function: externality.name(),
                                    param_num: 2,
                                    error: error.utf8_error(),
                                },
                                prototype: self.inner.into_prototype(),
                            };
                        }
                    };

                    return ExternalsVm::LogEmit(LogEmit {
                        inner: self.inner,
                        log_entry,
                    });
                }
            }
        }
    }
}

impl fmt::Debug for ReadyToRun {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ReadyToRun").finish()
    }
}

/// Function execution has succeeded. Contains the return value of the call.
pub struct Finished {
    inner: Inner,

    /// Value returned by the VM.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    value: Vec<u8>,
}

impl Finished {
    /// Returns the value the called function has returned.
    pub fn value(&self) -> &[u8] {
        &self.value
    }

    /// Turns the virtual machine back into a prototype.
    pub fn into_prototype(self) -> ExternalsVmPrototype {
        self.inner.into_prototype()
    }
}

impl fmt::Debug for Finished {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Finished").finish()
    }
}

/// Must provide the value of a storage entry.
pub struct ExternalStorageGet {
    inner: Inner,

    /// Function currently being called by the Wasm code. Refers to an index within
    /// [`Inner::registered_functions`].
    calling: usize,

    /// Used only for the `ext_storage_read_version_1` function. Stores the pointer where the
    /// output should be stored.
    value_out_ptr: Option<u32>,

    /// Key whose value must be loaded.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    key: Vec<u8>,
    /// Offset within the value that the Wasm VM requires.
    offset: u32,
    /// Maximum size that the Wasm VM would accept.
    max_size: u32,
}

impl ExternalStorageGet {
    /// Returns the key whose value must be provided back with [`ExternalStorageGet::resume`].
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Offset within the value that is requested.
    pub fn offset(&self) -> u32 {
        self.offset
    }

    /// Maximum size of the value to pass back.
    ///
    /// > **Note**: This can be 0 if we only want to know whether a value exists.
    pub fn max_size(&self) -> u32 {
        self.max_size
    }

    /// Same as [`ExternalStorageGet::resume`], but passes the full value, without taking the
    /// offset and maximum size into account.
    ///
    /// This is a convenient function that automatically applies the offset and maximum size, to
    /// use when the full storage value is already present in memory.
    pub fn resume_full_value(self, value: Option<&[u8]>) -> ExternalsVm {
        if let Some(value) = value {
            if usize::try_from(self.offset).unwrap() < value.len() {
                let value = &value[usize::try_from(self.offset).unwrap()..];
                if usize::try_from(self.max_size).unwrap() < value.len() {
                    let value = &value[..usize::try_from(self.max_size).unwrap()];
                    self.resume(Some(value))
                } else {
                    self.resume(Some(value))
                }
            } else {
                self.resume(Some(&[]))
            }
        } else {
            self.resume(None)
        }
    }

    /// Writes the storage value in the Wasm VM's memory and prepares the virtual machine to
    /// resume execution.
    ///
    /// The value to provide must be the value of that key starting at the offset returned by
    /// [`ExternalStorageGet::offset`]. If the offset is out of range, an empty slice must be
    /// passed.
    ///
    /// The value must not be longer than what [`ExternalStorageGet::max_size`] returns.
    ///
    /// # Panic
    ///
    /// Panics if the value is longer than what [`ExternalStorageGet::max_size`] returns.
    ///
    pub fn resume(self, value: Option<&[u8]>) -> ExternalsVm {
        self.resume_vectored(value.as_ref().map(iter::once))
    }

    /// Similar to [`ExternalStorageGet::resume`], but allows passing the value as a list of
    /// buffers whose concatenation forms the actual value.
    ///
    /// # Panic
    ///
    /// See [`ExternalStorageGet::resume`].
    ///
    pub fn resume_vectored(
        mut self,
        value: Option<impl Iterator<Item = impl AsRef<[u8]>> + Clone>,
    ) -> ExternalsVm {
        let externality = self.inner.registered_functions[self.calling];
        match externality {
            Externality::ext_storage_get_version_1 => {
                if let Some(value) = value {
                    // Writing `Some(value)`.
                    let value_len = value.clone().fold(0, |a, b| a + b.as_ref().len());
                    let value_len_enc = util::encode_scale_compact_usize(value_len);
                    self.inner.alloc_write_and_return_pointer_size(
                        externality.name(),
                        iter::once(&[1][..])
                            .chain(iter::once(value_len_enc.as_ref()))
                            .map(either::Left)
                            .chain(value.map(either::Right)),
                    )
                } else {
                    // Write a SCALE-encoded `None`.
                    self.inner
                        .alloc_write_and_return_pointer_size(externality.name(), iter::once(&[0]))
                }
            }
            Externality::ext_storage_read_version_1 => {
                let outcome = if let Some(value) = value {
                    let written =
                        u32::try_from(value.clone().fold(0, |a, b| a + b.as_ref().len())).unwrap();
                    assert!(written <= self.max_size);
                    // TODO: don't unwrap!
                    let mut offset = self.value_out_ptr.unwrap();
                    for value in value {
                        let value = value.as_ref();
                        self.inner.vm.write_memory(offset, value).unwrap();
                        offset += u32::try_from(value.len()).unwrap();
                    }
                    // TODO: while the specs mention that `written` should be returned,
                    // substrate instead returns the total length of the read value;
                    // see https://github.com/paritytech/substrate/pull/7084
                    Some(written)
                } else {
                    None
                };

                let outcome_encoded = parity_scale_codec::Encode::encode(&outcome);
                return self.inner.alloc_write_and_return_pointer_size(
                    externality.name(),
                    iter::once(&outcome_encoded),
                );
            }
            Externality::ext_storage_exists_version_1 => {
                return ExternalsVm::ReadyToRun(ReadyToRun {
                    inner: self.inner,
                    resume_value: Some(if value.is_some() {
                        vm::WasmValue::I32(1)
                    } else {
                        vm::WasmValue::I32(0)
                    }),
                })
            }
            _ => unreachable!(),
        }
    }
}

impl fmt::Debug for ExternalStorageGet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ExternalStorageGet").finish()
    }
}

/// Must set the value of a storage entry.
pub struct ExternalStorageSet {
    inner: Inner,

    /// Key whose value must be set.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    key: Vec<u8>,

    /// Value to set.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    value: Option<Vec<u8>>,
}

impl ExternalStorageSet {
    /// Returns the key whose value must be set.
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Returns the value to set.
    ///
    /// If `None` is returned, the key should be removed from the storage entirely.
    pub fn value(&self) -> Option<&[u8]> {
        self.value.as_ref().map(|b| &b[..])
    }

    /// Resumes execution after having set the value.
    pub fn resume(self) -> ExternalsVm {
        ExternalsVm::ReadyToRun(ReadyToRun {
            inner: self.inner,
            resume_value: None,
        })
    }
}

impl fmt::Debug for ExternalStorageSet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ExternalStorageSet").finish()
    }
}

/// Must load a storage value, treat it as if it was a SCALE-encoded container, and put `value`
/// at the end of the container, increasing the number of elements.
///
/// If there isn't any existing value of if the existing value isn't actually a SCALE-encoded
/// container, store a 1-size container with the `value`.
///
/// # Details
///
/// The SCALE encoding encodes containers as a SCALE-compact-encoded length followed with the
/// SCALE-encoded items one after the other. For example, a container of two elements is stored
/// as the number `2` followed with the two items.
///
/// This change consists in taking an existing value and assuming that it is a SCALE-encoded
/// container. This can be done as decoding a SCALE-compact-encoded number at the start of
/// the existing encoded value. One most then increment that number and puting `value` at the
/// end of the encoded value.
///
/// It is not necessary to decode `value` as is assumed that is already encoded in the same
/// way as the other items in the container.
pub struct ExternalStorageAppend {
    inner: Inner,

    /// Key whose value must be set.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    key: Vec<u8>,

    /// Value to append to the entry.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    value: Vec<u8>,
}

impl ExternalStorageAppend {
    /// Returns the key whose value must be set.
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Returns the value to append.
    pub fn value(&self) -> &[u8] {
        &self.value
    }

    /// Resumes execution after having set the value.
    pub fn resume(self) -> ExternalsVm {
        ExternalsVm::ReadyToRun(ReadyToRun {
            inner: self.inner,
            resume_value: None,
        })
    }
}

impl fmt::Debug for ExternalStorageAppend {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ExternalStorageAppend").finish()
    }
}

/// Must remove from the storage all keys which start with a certain prefix.
pub struct ExternalStorageClearPrefix {
    inner: Inner,

    /// Prefix of the keys to remove.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    prefix: Vec<u8>,
}

impl ExternalStorageClearPrefix {
    /// Returns the prefix whose keys must be removed.
    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    /// Resumes execution after having set the value.
    pub fn resume(self) -> ExternalsVm {
        ExternalsVm::ReadyToRun(ReadyToRun {
            inner: self.inner,
            resume_value: None,
        })
    }
}

impl fmt::Debug for ExternalStorageClearPrefix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ExternalStorageClearPrefix").finish()
    }
}

/// Must provide the trie root hash of the storage.
pub struct ExternalStorageRoot {
    inner: Inner,
}

impl ExternalStorageRoot {
    /// Writes the trie root hash to the Wasm VM and prepares it for resume.
    pub fn resume(self, hash: &[u8; 32]) -> ExternalsVm {
        self.inner.alloc_write_and_return_pointer_size(
            Externality::ext_storage_root_version_1.name(),
            iter::once(hash),
        )
    }
}

impl fmt::Debug for ExternalStorageRoot {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ExternalStorageRoot").finish()
    }
}

/// Must provide the trie root hash of the changes trie.
pub struct ExternalStorageChangesRoot {
    inner: Inner,
}

impl ExternalStorageChangesRoot {
    /// Writes the trie root hash to the Wasm VM and prepares it for resume.
    // TODO: document why it can be `None`
    pub fn resume(self, hash: Option<&[u8; 32]>) -> ExternalsVm {
        if let Some(hash) = hash {
            // Writing the `Some` of the SCALE-encoded `Option`.
            self.inner.alloc_write_and_return_pointer_size(
                Externality::ext_storage_changes_root_version_1.name(),
                iter::once(&[1][..]).chain(iter::once(&hash[..])),
            )
        } else {
            // Writing a SCALE-encoded `None`.
            self.inner.alloc_write_and_return_pointer_size(
                Externality::ext_storage_changes_root_version_1.name(),
                iter::once(&[0][..]),
            )
        }
    }
}

impl fmt::Debug for ExternalStorageChangesRoot {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ExternalStorageChangesRoot").finish()
    }
}

/// Must provide the storage key that follows, in lexicographic order, a specific one.
pub struct ExternalStorageNextKey {
    inner: Inner,

    /// Key whose follow-up must be provided.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    key: Vec<u8>,
}

impl ExternalStorageNextKey {
    /// Returns the key whose following key must be returned.
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Writes the follow-up key in the Wasm VM memory and prepares it for execution.
    ///
    /// Must be passed `None` if the key is the last one in the storage.
    pub fn resume(self, follow_up: Option<&[u8]>) -> ExternalsVm {
        if let Some(follow_up) = follow_up {
            let value_len_enc = util::encode_scale_compact_usize(follow_up.len());
            self.inner.alloc_write_and_return_pointer_size(
                Externality::ext_storage_next_key_version_1.name(),
                iter::once(&[1][..])
                    .chain(iter::once(value_len_enc.as_ref()))
                    .chain(iter::once(follow_up)),
            )
        } else {
            // Write a SCALE-encoded `None`.
            self.inner.alloc_write_and_return_pointer_size(
                Externality::ext_storage_next_key_version_1.name(),
                iter::once(&[0]),
            )
        }
    }
}

impl fmt::Debug for ExternalStorageNextKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ExternalStorageNextKey").finish()
    }
}

/// Must provide the runtime version obtained by calling the `Core_version` entry point of a Wasm
/// blob.
pub struct CallRuntimeVersion {
    inner: Inner,

    /// Wasm code whose runtime version must be provided.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    wasm_blob: Vec<u8>,
}

impl CallRuntimeVersion {
    /// Returns the Wasm code whose runtime version must be provided.
    pub fn wasm_code(&self) -> &[u8] {
        &self.wasm_blob
    }

    /// Writes the SCALE-encoded runtime version to the memory and prepares for execution.
    ///
    /// If an error happened during the execution (such as an invalid Wasm binary code), pass
    /// an `Err`.
    pub fn resume(self, scale_encoded_runtime_version: Result<&[u8], ()>) -> ExternalsVm {
        // TODO: don't allocate a Vec here
        let scale_encoded_runtime_version =
            parity_scale_codec::Encode::encode(&scale_encoded_runtime_version.ok());
        self.inner.alloc_write_and_return_pointer_size(
            Externality::ext_misc_runtime_version_version_1.name(),
            iter::once(scale_encoded_runtime_version),
        )
    }
}

impl fmt::Debug for CallRuntimeVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("CallRuntimeVersion").finish()
    }
}

/// Must set the value of the offchain storage.
pub struct ExternalOffchainStorageSet {
    inner: Inner,

    /// Key whose value must be set.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    key: Vec<u8>,

    /// Value to set.
    // TODO: This should be a value length and pointer intead, so that we can read from the
    //       VM's memory without copying. However the underlying Wasm VM code doesn't support
    //       reading without copies.
    value: Option<Vec<u8>>,
}

impl ExternalOffchainStorageSet {
    /// Returns the key whose value must be set.
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Returns the value to set.
    ///
    /// If `None` is returned, the key should be removed from the storage entirely.
    pub fn value(&self) -> Option<&[u8]> {
        self.value.as_ref().map(|b| &b[..])
    }

    /// Resumes execution after having set the value.
    pub fn resume(self) -> ExternalsVm {
        ExternalsVm::ReadyToRun(ReadyToRun {
            inner: self.inner,
            resume_value: None,
        })
    }
}

impl fmt::Debug for ExternalOffchainStorageSet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ExternalOffchainStorageSet").finish()
    }
}

/// Report about a log entry being emitted.
///
/// Use the implementation of [`fmt::Display`] to obtain the log entry. For exmaple, you can
/// call [`alloc::string::ToString::to_string`] to turn it into a `String`.
pub struct LogEmit {
    inner: Inner,
    log_entry: String,
}

impl LogEmit {
    /// Resumes execution after having set the value.
    pub fn resume(self) -> ExternalsVm {
        ExternalsVm::ReadyToRun(ReadyToRun {
            inner: self.inner,
            resume_value: None,
        })
    }
}

impl fmt::Display for LogEmit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.log_entry)
    }
}

impl fmt::Debug for LogEmit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("LogEmit")
            .field("message", &self.log_entry)
            .finish()
    }
}

/// Declares the start of a transaction.
pub struct StartStorageTransaction {
    inner: Inner,
}

impl StartStorageTransaction {
    /// Resumes execution after having acknowledged the event.
    pub fn resume(self) -> ExternalsVm {
        ExternalsVm::ReadyToRun(ReadyToRun {
            inner: self.inner,
            resume_value: None,
        })
    }
}

/// Declares the end of a transaction.
pub struct EndStorageTransaction {
    inner: Inner,
}

impl EndStorageTransaction {
    /// Resumes execution after having acknowledged the event.
    pub fn resume(self) -> ExternalsVm {
        ExternalsVm::ReadyToRun(ReadyToRun {
            inner: self.inner,
            resume_value: None,
        })
    }
}

/// Running virtual machine. Shared between all the variants in [`ExternalsVm`].
struct Inner {
    /// Inner lower-level virtual machine.
    vm: vm::VirtualMachine,

    /// Initial value of the `__heap_base` global in the Wasm module. Used to initialize the memory
    /// allocator in case we need to rebuild the VM.
    heap_base: u32,

    /// If true, a transaction has been started using `ext_storage_start_transaction_version_1`.
    /// No further transaction start is allowed before the current one ends.
    within_storage_transaction: bool,

    /// See [`ExternalsVmPrototype::registered_functions`].
    registered_functions: Vec<Externality>,

    /// Memory allocator in order to answer the calls to `malloc` and `free`.
    allocator: allocator::FreeingBumpHeapAllocator,
}

impl Inner {
    /// Uses the memory allocator to allocate some memory for the given data, writes the data in
    /// memory, and returns an [`ExternalsVm`] ready for the Wasm externality return.
    ///
    /// The data is passed as a list of chunks. These chunks will be laid out lineraly in memory.
    ///
    /// The function name passed as parameter is used for error-reporting reasons.
    ///
    /// # Panic
    ///
    /// Must only be called while the Wasm is handling an externality.
    ///
    fn alloc_write_and_return_pointer_size(
        mut self,
        function_name: &'static str,
        data: impl Iterator<Item = impl AsRef<[u8]>> + Clone,
    ) -> ExternalsVm {
        let mut data_len = 0u32;
        for chunk in data.clone() {
            data_len = data_len
                .saturating_add(u32::try_from(chunk.as_ref().len()).unwrap_or(u32::max_value()));
        }

        let dest_ptr = match self
            .allocator
            .allocate(&mut MemAccess(&mut self.vm), data_len)
        {
            Ok(p) => p,
            Err(_) => {
                return ExternalsVm::Error {
                    error: Error::OutOfMemory {
                        function: function_name,
                        requested_size: data_len,
                    },
                    prototype: self.into_prototype(),
                }
            }
        };

        let mut ptr_iter = dest_ptr;
        for chunk in data {
            let chunk = chunk.as_ref();
            self.vm.write_memory(ptr_iter, chunk).unwrap();
            ptr_iter += u32::try_from(chunk.len()).unwrap_or(u32::max_value());
        }

        let ret_val = (u64::from(data_len) << 32) | u64::from(dest_ptr);
        let ret_val = i64::from_ne_bytes(ret_val.to_ne_bytes());

        ReadyToRun {
            inner: self,
            resume_value: Some(vm::WasmValue::I64(ret_val)),
        }
        .into()
    }

    /// Uses the memory allocator to allocate some memory for the given data, writes the data in
    /// memory, and returns an [`ExternalsVm`] ready for the Wasm externality return.
    ///
    /// The data is passed as a list of chunks. These chunks will be laid out lineraly in memory.
    ///
    /// The function name passed as parameter is used for error-reporting reasons.
    ///
    /// # Panic
    ///
    /// Must only be called while the Wasm is handling an externality.
    ///
    fn alloc_write_and_return_pointer(
        mut self,
        function_name: &'static str,
        data: impl Iterator<Item = impl AsRef<[u8]>> + Clone,
    ) -> ExternalsVm {
        let mut data_len = 0u32;
        for chunk in data.clone() {
            data_len = data_len
                .saturating_add(u32::try_from(chunk.as_ref().len()).unwrap_or(u32::max_value()));
        }

        let dest_ptr = match self
            .allocator
            .allocate(&mut MemAccess(&mut self.vm), data_len)
        {
            Ok(p) => p,
            Err(_) => {
                return ExternalsVm::Error {
                    error: Error::OutOfMemory {
                        function: function_name,
                        requested_size: data_len,
                    },
                    prototype: self.into_prototype(),
                }
            }
        };

        let mut ptr_iter = dest_ptr;
        for chunk in data {
            let chunk = chunk.as_ref();
            self.vm.write_memory(ptr_iter, chunk).unwrap();
            ptr_iter += u32::try_from(chunk.len()).unwrap_or(u32::max_value());
        }

        let ret_val = i32::from_ne_bytes(dest_ptr.to_ne_bytes());
        ReadyToRun {
            inner: self,
            resume_value: Some(vm::WasmValue::I32(ret_val)),
        }
        .into()
    }

    /// Turns the virtual machine back into a prototype.
    fn into_prototype(self) -> ExternalsVmPrototype {
        ExternalsVmPrototype {
            vm_proto: self.vm.into_prototype(),
            heap_base: self.heap_base,
            registered_functions: self.registered_functions,
        }
    }
}

/// Error that can happen when initializing a VM.
#[derive(Debug, derive_more::From, derive_more::Display)]
pub enum NewErr {
    /// Error while initializing the virtual machine.
    #[display(fmt = "Error while initializing the virtual machine: {}", _0)]
    VirtualMachine(vm::NewErr),
    /// The size of the input data is too large.
    DataSizeOverflow,
    /// Couldn't find the `__heap_base` symbol in the Wasm code.
    HeapBaseNotFound,
}

/// Reason why the Wasm blob isn't conforming to the runtime environment.
#[derive(Debug, Clone, derive_more::Display)]
pub enum Error {
    /// Error in the Wasm code execution.
    Trapped,
    /// A non-`i64` value has been returned by the Wasm entry point.
    #[display(fmt = "A non-I64 value has been returned: {:?}", actual)]
    BadReturnValue {
        /// Type that has actually gotten returned. `None` for "void".
        actual: Option<vm::ValueType>,
    },
    /// The pointer and size returned by the Wasm entry point function are invalid.
    #[display(fmt = "The pointer and size returned by the function are invalid")]
    ReturnedPtrOutOfRange {
        /// Pointer that got returned.
        pointer: u32,
        /// Size that got returned.
        size: u32,
        /// Size of the virtual memory.
        memory_size: u32,
    },
    /// An externality wants to returns a certain value, but the Wasm code expects a different one.
    // TODO: indicate function and actual/expected types
    ReturnValueTypeMismatch,
    /// Mismatch between the number of parameters expected and the actual number.
    #[display(
        fmt = "Mismatch in parameters count: {}, expected = {}, actual = {}",
        function,
        expected,
        actual
    )]
    ParamsCountMismatch {
        /// Name of the function being called whose number of parameters mismatches.
        function: &'static str,
        /// Expected number of parameters.
        expected: usize,
        /// Number of parameters that have been passed.
        actual: usize,
    },
    /// Failed to decode a SCALE-encoded parameter.
    // TODO: refactor and/or remove
    ParamDecodeError(parity_scale_codec::Error),
    /// The type of one of the parameters is wrong.
    #[display(
        fmt = "Type mismatch in parameter #{}: {}, expected = {:?}, actual = {:?}",
        param_num,
        function,
        expected,
        actual
    )]
    WrongParamTy {
        /// Name of the function being called where a type mismatch happens.
        function: &'static str,
        /// Index of the invalid parameter. The first parameter has index 0.
        param_num: usize,
        /// Type of the value that was expected.
        expected: vm::ValueType,
        /// Type of the value that got passed.
        actual: vm::ValueType,
    },
    /// One parameter is expected to point to a buffer, but the pointer is out
    /// of range of the memory of the Wasm VM.
    #[display(
        fmt = "Bad pointer for parameter #{} of {}: 0x{:x}, len = 0x{:x}",
        param_num,
        function,
        pointer,
        length
    )]
    ParamOutOfRange {
        /// Name of the function being called where a type mismatch happens.
        function: &'static str,
        /// Index of the invalid parameter. The first parameter has index 0.
        param_num: usize,
        /// Pointer passed as parameter.
        pointer: u32,
        /// Expected length of the buffer.
        ///
        /// Depending on the function, this can either be an implicit length
        /// or a length passed as parameter.
        length: u32,
    },
    /// One parameter is expected to point to a UTF-8 string, but the buffer
    /// isn't valid UTF-8.
    #[display(
        fmt = "UTF-8 error for parameter #{} of {}: {}",
        param_num,
        function,
        error
    )]
    Utf8Error {
        /// Name of the function being called where a type mismatch happens.
        function: &'static str,
        /// Index of the invalid parameter. The first parameter has index 0.
        param_num: usize,
        /// Decoding error that happened.
        error: core::str::Utf8Error,
    },
    /// Called `ext_storage_start_transaction_version_1` with a transaction was already in
    /// progress.
    #[display(fmt = "Attempted to start a transaction while one is already in progress")]
    NestedTransaction,
    /// Called `ext_storage_rollback_transaction_version_1` or
    /// `ext_storage_commit_transaction_version_1` but no transaction was in progress.
    #[display(fmt = "Attempted to end a transaction while none is in progress")]
    NoActiveTransaction,
    /// Execution has finished while a transaction started with
    /// `ext_storage_start_transaction_version_1` was still in progress.
    #[display(fmt = "Execution returned with a pending storage transaction")]
    FinishedWithPendingTransaction,
    /// Error when allocating memory for a return type.
    #[display(
        fmt = "Out of memory allocating 0x{:x} bytes during {}",
        requested_size,
        function
    )]
    OutOfMemory {
        /// Name of the function being called.
        function: &'static str,
        /// Size of the requested allocation.
        requested_size: u32,
    },
    /// Called `ext_allocator_free_version_1` with an invalid pointer.
    #[display(
        fmt = "Bad pointer passed to ext_allocator_free_version_1: 0x{:x}",
        pointer
    )]
    FreeError {
        /// Pointer that was expected to be free'd.
        pointer: u32,
    },
}

macro_rules! externalities {
    ($($ext:ident,)*) => {
        /// List of possible externalities.
        #[derive(Debug, Copy, Clone, PartialEq, Eq)]
        #[allow(non_camel_case_types)]
        enum Externality {
            $(
                $ext,
            )*
        }

        impl Externality {
            fn by_name(name: &str) -> Option<Self> {
                $(
                    if name == stringify!($ext) {
                        return Some(Externality::$ext);
                    }
                )*
                None
            }

            fn name(&self) -> &'static str {
                match self {
                    $(
                        Externality::$ext => stringify!($ext),
                    )*
                }
            }
        }
    };
}

externalities! {
    ext_storage_set_version_1,
    ext_storage_get_version_1,
    ext_storage_read_version_1,
    ext_storage_clear_version_1,
    ext_storage_exists_version_1,
    ext_storage_clear_prefix_version_1,
    ext_storage_root_version_1,
    ext_storage_changes_root_version_1,
    ext_storage_next_key_version_1,
    ext_storage_append_version_1,
    ext_storage_child_set_version_1,
    ext_storage_child_get_version_1,
    ext_storage_child_read_version_1,
    ext_storage_child_clear_version_1,
    ext_storage_child_storage_kill_version_1,
    ext_storage_child_exists_version_1,
    ext_storage_child_clear_prefix_version_1,
    ext_storage_child_root_version_1,
    ext_storage_child_next_key_version_1,
    ext_storage_start_transaction_version_1,
    ext_storage_rollback_transaction_version_1,
    ext_storage_commit_transaction_version_1,
    ext_default_child_storage_get_version_1,
    ext_default_child_storage_storage_kill_version_1,
    ext_default_child_storage_set_version_1,
    ext_default_child_storage_clear_version_1,
    ext_default_child_storage_root_version_1,
    ext_crypto_ed25519_public_keys_version_1,
    ext_crypto_ed25519_generate_version_1,
    ext_crypto_ed25519_sign_version_1,
    ext_crypto_ed25519_verify_version_1,
    ext_crypto_sr25519_public_keys_version_1,
    ext_crypto_sr25519_generate_version_1,
    ext_crypto_sr25519_sign_version_1,
    ext_crypto_sr25519_verify_version_1,
    ext_crypto_sr25519_verify_version_2,
    ext_crypto_secp256k1_ecdsa_recover_version_1,
    ext_crypto_secp256k1_ecdsa_recover_compressed_version_1,
    ext_crypto_start_batch_verify_version_1,
    ext_crypto_finish_batch_verify_version_1,
    ext_hashing_keccak_256_version_1,
    ext_hashing_sha2_256_version_1,
    ext_hashing_blake2_128_version_1,
    ext_hashing_blake2_256_version_1,
    ext_hashing_twox_64_version_1,
    ext_hashing_twox_128_version_1,
    ext_hashing_twox_256_version_1,
    ext_offchain_index_set_version_1,
    ext_offchain_index_clear_version_1,
    ext_offchain_is_validator_version_1,
    ext_offchain_submit_transaction_version_1,
    ext_offchain_network_state_version_1,
    ext_offchain_timestamp_version_1,
    ext_offchain_sleep_until_version_1,
    ext_offchain_random_seed_version_1,
    ext_offchain_local_storage_set_version_1,
    ext_offchain_local_storage_compare_and_set_version_1,
    ext_offchain_local_storage_get_version_1,
    ext_offchain_http_request_start_version_1,
    ext_offchain_http_request_add_header_version_1,
    ext_offchain_http_request_write_body_version_1,
    ext_offchain_http_response_wait_version_1,
    ext_offchain_http_response_headers_version_1,
    ext_offchain_http_response_read_body_version_1,
    ext_sandbox_instantiate_version_1,
    ext_sandbox_invoke_version_1,
    ext_sandbox_memory_new_version_1,
    ext_sandbox_memory_get_version_1,
    ext_sandbox_memory_set_version_1,
    ext_sandbox_memory_teardown_version_1,
    ext_sandbox_instance_teardown_version_1,
    ext_sandbox_get_global_val_version_1,
    ext_trie_blake2_256_root_version_1,
    ext_trie_blake2_256_ordered_root_version_1,
    ext_misc_chain_id_version_1,
    ext_misc_print_num_version_1,
    ext_misc_print_utf8_version_1,
    ext_misc_print_hex_version_1,
    ext_misc_runtime_version_version_1,
    ext_allocator_malloc_version_1,
    ext_allocator_free_version_1,
    ext_logging_log_version_1,
}

// Glue between the `allocator` module and the `vm` module.
struct MemAccess<'a>(&'a mut vm::VirtualMachine);
impl<'a> allocator::Memory for MemAccess<'a> {
    fn read_le_u64(&self, ptr: u32) -> Result<u64, allocator::Error> {
        let bytes = self.0.read_memory(ptr, 8).unwrap(); // TODO: convert error
        Ok(u64::from_le_bytes(
            <[u8; 8]>::try_from(bytes.as_ref()).unwrap(),
        ))
    }

    fn write_le_u64(&mut self, ptr: u32, val: u64) -> Result<(), allocator::Error> {
        let bytes = val.to_le_bytes();
        self.0.write_memory(ptr, &bytes).unwrap(); // TODO: convert error instead
        Ok(())
    }

    fn size(&self) -> u32 {
        self.0.memory_size()
    }
}

#[cfg(test)]
mod tests {
    use super::ExternalsVm;

    #[test]
    fn is_send() {
        fn req<T: Send>() {}
        req::<ExternalsVm>();
    }
}
