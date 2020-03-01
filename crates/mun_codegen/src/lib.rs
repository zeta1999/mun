/// This library generates machine code from HIR using inkwell which is a safe wrapper around LLVM.
mod code_gen;
mod db;
#[macro_use]
mod ir;
pub(crate) mod symbols;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod test;

pub(crate) mod intrinsics;
pub(crate) mod type_info;

pub use inkwell::{builder, context::Context, module::Module, values, OptimizationLevel};

pub use crate::{
    code_gen::write_module_shared_object,
    db::{IrDatabase, IrDatabaseStorage},
};

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct CodeGenParams {
    /// Whether generated code should support extern function calls.
    /// This allows function parameters with `struct(value)` types to be marshalled.
    is_extern: bool,
}
