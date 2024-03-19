// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use move_binary_format::errors::PartialVMError;
use move_core_types::{
    gas_algebra::InternalGas, identifier::Identifier, language_storage::ModuleId,
};
use move_vm_types::{loaded_data::runtime_types::Type, values::Value};
use smallvec::SmallVec;

/// Saner representation of a native function error.
#[allow(unused)]
pub enum SafeNativeError {
    /// Indicating that the native function has aborted due to some (user) errors.
    ///
    /// Equivalent to aborting in a regular Move function, so the same error convention should
    /// be followed.
    Abort { abort_code: u64 },

    /// Indicating that the native function has run out of gas during execution.
    ///
    /// This will cause the VM to deduct all the remaining balance and abort the transaction,
    /// so use it carefully!
    /// Normally this should only be triggered by `SafeNativeContext::charge()` and you should
    /// not return this manually without a good reason.
    OutOfGas,

    /// Indicating that the native function ran into some internal errors that shall not normally
    /// be triggerable by user inputs.
    InvariantViolation(PartialVMError),

    /// Indicating the native function will result in a switch in control flow.
    CallFunction {
        cost: InternalGas,
        module_name: ModuleId,
        func_name: Identifier,
        ty_args: Vec<Type>,
        args: SmallVec<[Value; 1]>,
    },
}

// Allows us to keep using the `?` operator on function calls that return `PartialVMResult` inside safe natives.
//
// TODO(Gas): This automatic conversion is VERY PROBLEMATIC as it makes it extremely easy to
//            accidentally propagate a non-invariant violation, which is a violation of the
//            contract.
//
//            We are actively seeking to remove this implementation.
//            Please help us stop the bleed by not using this conversion.
impl From<PartialVMError> for SafeNativeError {
    fn from(e: PartialVMError) -> Self {
        SafeNativeError::InvariantViolation(e)
    }
}

pub type SafeNativeResult<T> = Result<T, SafeNativeError>;
