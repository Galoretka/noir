//! This module is an abstraction layer over `Brillig`.
//! To allow for separation of concerns, it knows nothing
//! about SSA types, and can therefore be tested independently.
//! `brillig_gen` is therefore the module which combines both
//! ssa types and types in this module.
//! A similar paradigm can be seen with the `acir_ir` module.
//!
//! The brillig ir provides instructions and codegens.
//! The instructions are low level operations that are printed via debug_show.
//! They should emit few opcodes. Codegens on the other hand orchestrate the
//! low level instructions to emit the desired high level operation.
pub mod artifact;
pub(crate) mod brillig_variable;
pub(crate) mod debug_show;
pub(crate) mod procedures;
pub(crate) mod registers;

mod codegen_binary;
mod codegen_calls;
mod codegen_control_flow;
mod codegen_intrinsic;
mod codegen_memory;
mod codegen_stack;
mod entry_point;
mod instructions;

use artifact::Label;
use brillig_variable::SingleAddrVariable;
pub(crate) use instructions::BrilligBinaryOp;
use noirc_errors::call_stack::CallStackId;
use registers::{RegisterAllocator, ScratchSpace};

use self::{artifact::BrilligArtifact, debug_show::DebugToString, registers::Stack};
use acvm::{
    AcirField,
    acir::brillig::{MemoryAddress, Opcode as BrilligOpcode},
};
use debug_show::DebugShow;

use super::{BrilligOptions, FunctionId, GlobalSpace, ProcedureId};

/// The Brillig VM does not apply a limit to the memory address space,
/// As a convention, we take use 32 bits. This means that we assume that
/// memory has 2^32 memory slots.
pub(crate) const BRILLIG_MEMORY_ADDRESSING_BIT_SIZE: u32 = 32;

// Registers reserved in runtime for special purposes.
pub(crate) enum ReservedRegisters {
    /// This register stores the stack pointer. All relative memory addresses are relative to this pointer.
    StackPointer = 0,
    /// This register stores the free memory pointer. Allocations must be done after this pointer.
    FreeMemoryPointer = 1,
    /// This register stores a 1_usize constant.
    UsizeOne = 2,
}

impl ReservedRegisters {
    /// The number of reserved registers. These are allocated in the first memory positions.
    /// The stack should start after the reserved registers.
    const NUM_RESERVED_REGISTERS: usize = 3;

    /// Returns the length of the reserved registers
    pub(crate) fn len() -> usize {
        Self::NUM_RESERVED_REGISTERS
    }

    pub(crate) fn stack_pointer() -> MemoryAddress {
        MemoryAddress::direct(ReservedRegisters::StackPointer as usize)
    }

    pub(crate) fn free_memory_pointer() -> MemoryAddress {
        MemoryAddress::direct(ReservedRegisters::FreeMemoryPointer as usize)
    }

    pub(crate) fn usize_one() -> MemoryAddress {
        MemoryAddress::direct(ReservedRegisters::UsizeOne as usize)
    }
}

/// Brillig context object that is used while constructing the
/// Brillig bytecode.
pub(crate) struct BrilligContext<F, Registers> {
    obj: BrilligArtifact<F>,
    /// Tracks register allocations
    registers: Registers,
    /// Context label, must be unique with respect to the function
    /// being linked.
    context_label: Label,
    /// Section label, used to separate sections of code
    current_section: usize,
    /// Stores the next available section
    next_section: usize,
    /// IR printer
    debug_show: DebugShow,
    /// Whether this context can call procedures or not.
    /// This is used to prevent a procedure from calling another procedure.
    can_call_procedures: bool,
    /// Insert extra assertions that we expect to be true, at the cost of larger bytecode size.
    enable_debug_assertions: bool,
    /// Count the number of arrays that are copied, and output this to stdout
    count_arrays_copied: bool,

    globals_memory_size: Option<usize>,
}

impl<F, R> BrilligContext<F, R> {
    /// Enable the insertion of bytecode with extra assertions during testing.
    pub(crate) fn enable_debug_assertions(&self) -> bool {
        self.enable_debug_assertions
    }

    /// Returns the address of the implicit debug variable containing the count of
    /// implicitly copied arrays as a result of RC's copy on write semantics.
    pub(crate) fn array_copy_counter_address(&self) -> MemoryAddress {
        assert!(
            self.count_arrays_copied,
            "`count_arrays_copied` is not set, so the array copy counter does not exist"
        );

        // The copy counter is always put in the first global slot
        MemoryAddress::Direct(GlobalSpace::start())
    }

    pub(crate) fn count_array_copies(&self) -> bool {
        self.count_arrays_copied
    }

    /// Set the globals memory size if it is not already set.
    /// If it is already set, this will assert that the two values must be equal.
    pub(crate) fn set_globals_memory_size(&mut self, new_size: Option<usize>) {
        if self.globals_memory_size.is_some() {
            assert_eq!(
                self.globals_memory_size, new_size,
                "Tried to change globals_memory_size to a different value"
            );
        }
        self.globals_memory_size = new_size;
    }
}

/// Regular brillig context to codegen user defined functions
impl<F: AcirField + DebugToString> BrilligContext<F, Stack> {
    pub(crate) fn new(options: &BrilligOptions) -> BrilligContext<F, Stack> {
        BrilligContext {
            obj: BrilligArtifact::default(),
            registers: Stack::new(),
            context_label: Label::entrypoint(),
            current_section: 0,
            next_section: 1,
            debug_show: DebugShow::new(options.enable_debug_trace),
            enable_debug_assertions: options.enable_debug_assertions,
            count_arrays_copied: options.enable_array_copy_counter,
            can_call_procedures: true,
            globals_memory_size: None,
        }
    }
}

impl<F: AcirField + DebugToString, Registers: RegisterAllocator> BrilligContext<F, Registers> {
    /// Splits a two's complement signed integer in the sign bit and the absolute value.
    /// For example, -6 i8 (11111010) is split to 00000110 (6, absolute value) and 1 (is_negative).
    pub(crate) fn absolute_value(
        &mut self,
        num: SingleAddrVariable,
        absolute_value: SingleAddrVariable,
        result_is_negative: SingleAddrVariable,
    ) {
        let max_positive = self
            .make_constant_instruction(((1_u128 << (num.bit_size - 1)) - 1).into(), num.bit_size);

        // Compute if num is negative
        self.binary_instruction(max_positive, num, result_is_negative, BrilligBinaryOp::LessThan);

        // Two's complement of num
        let zero = self.make_constant_instruction(0_usize.into(), num.bit_size);
        let twos_complement = SingleAddrVariable::new(self.allocate_register(), num.bit_size);
        self.binary_instruction(zero, num, twos_complement, BrilligBinaryOp::Sub);

        // absolute_value = result_is_negative ? twos_complement : num
        self.codegen_branch(result_is_negative.address, |ctx, is_negative| {
            if is_negative {
                ctx.mov_instruction(absolute_value.address, twos_complement.address);
            } else {
                ctx.mov_instruction(absolute_value.address, num.address);
            }
        });

        self.deallocate_single_addr(zero);
        self.deallocate_single_addr(max_positive);
        self.deallocate_single_addr(twos_complement);
    }

    pub(crate) fn convert_signed_division(
        &mut self,
        left: SingleAddrVariable,
        right: SingleAddrVariable,
        result: SingleAddrVariable,
    ) {
        let left_is_negative = SingleAddrVariable::new(self.allocate_register(), 1);
        let left_abs_value = SingleAddrVariable::new(self.allocate_register(), left.bit_size);

        let right_is_negative = SingleAddrVariable::new(self.allocate_register(), 1);
        let right_abs_value = SingleAddrVariable::new(self.allocate_register(), right.bit_size);

        let result_is_negative = SingleAddrVariable::new(self.allocate_register(), 1);

        // Compute both absolute values
        self.absolute_value(left, left_abs_value, left_is_negative);
        self.absolute_value(right, right_abs_value, right_is_negative);

        // Perform the division on the absolute values
        self.binary_instruction(
            left_abs_value,
            right_abs_value,
            result,
            BrilligBinaryOp::UnsignedDiv,
        );

        // Compute result sign
        self.binary_instruction(
            left_is_negative,
            right_is_negative,
            result_is_negative,
            BrilligBinaryOp::Xor,
        );

        // If result has to be negative, perform two's complement
        self.codegen_if(result_is_negative.address, |ctx| {
            let zero = ctx.make_constant_instruction(0_usize.into(), result.bit_size);
            ctx.binary_instruction(zero, result, result, BrilligBinaryOp::Sub);
            ctx.deallocate_single_addr(zero);
        });

        self.deallocate_single_addr(left_is_negative);
        self.deallocate_single_addr(left_abs_value);
        self.deallocate_single_addr(right_is_negative);
        self.deallocate_single_addr(right_abs_value);
        self.deallocate_single_addr(result_is_negative);
    }
}

/// Special brillig context to codegen compiler intrinsic shared procedures
impl<F: AcirField + DebugToString> BrilligContext<F, ScratchSpace> {
    pub(crate) fn new_for_procedure(
        procedure_id: ProcedureId,
        options: &BrilligOptions,
    ) -> BrilligContext<F, ScratchSpace> {
        let mut obj = BrilligArtifact::default();
        obj.procedure = Some(procedure_id);
        BrilligContext {
            obj,
            registers: ScratchSpace::new(),
            context_label: Label::entrypoint(),
            current_section: 0,
            next_section: 1,
            debug_show: DebugShow::new(options.enable_debug_trace),
            enable_debug_assertions: options.enable_debug_assertions,
            count_arrays_copied: options.enable_array_copy_counter,
            can_call_procedures: false,
            globals_memory_size: None,
        }
    }
}

/// Special brillig context to codegen global values initialization
impl<F: AcirField + DebugToString> BrilligContext<F, GlobalSpace> {
    pub(crate) fn new_for_global_init(
        options: &BrilligOptions,
        entry_point: FunctionId,
    ) -> BrilligContext<F, GlobalSpace> {
        BrilligContext {
            obj: BrilligArtifact::default(),
            registers: GlobalSpace::new(),
            context_label: Label::globals_init(entry_point),
            current_section: 0,
            next_section: 1,
            debug_show: DebugShow::new(options.enable_debug_trace),
            enable_debug_assertions: options.enable_debug_assertions,
            count_arrays_copied: options.enable_array_copy_counter,
            can_call_procedures: false,
            globals_memory_size: None,
        }
    }

    pub(crate) fn global_space_size(&self) -> usize {
        // `GlobalSpace::start()` is inclusive so we must add one to get the accurate total global memory size
        (self.registers.max_memory_address() + 1) - GlobalSpace::start()
    }
}

impl<F: AcirField + DebugToString, Registers: RegisterAllocator> BrilligContext<F, Registers> {
    /// Adds a brillig instruction to the brillig byte code
    fn push_opcode(&mut self, opcode: BrilligOpcode<F>) {
        self.obj.push_opcode(opcode);
    }

    /// Returns the artifact
    pub(crate) fn artifact(self) -> BrilligArtifact<F> {
        self.obj
    }

    /// Sets a current call stack that the next pushed opcodes will be associated with.
    pub(crate) fn set_call_stack(&mut self, call_stack: CallStackId) {
        self.obj.set_call_stack(call_stack);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::vec;

    use acvm::acir::brillig::{
        BitSize, ForeignCallParam, ForeignCallResult, HeapVector, IntegerBitSize, MemoryAddress,
        ValueOrArray,
    };
    use acvm::brillig_vm::brillig::HeapValueType;
    use acvm::brillig_vm::{VM, VMStatus};
    use acvm::{BlackBoxFunctionSolver, BlackBoxResolutionError, FieldElement};

    use crate::brillig::BrilligOptions;
    use crate::brillig::brillig_ir::{BrilligBinaryOp, BrilligContext};
    use crate::ssa::ir::function::FunctionId;

    use super::artifact::{BrilligParameter, GeneratedBrillig, Label, LabelType};
    use super::procedures::compile_procedure;
    use super::registers::Stack;
    use super::{BrilligOpcode, ReservedRegisters};

    pub(crate) struct DummyBlackBoxSolver;

    impl BlackBoxFunctionSolver<FieldElement> for DummyBlackBoxSolver {
        fn pedantic_solving(&self) -> bool {
            true
        }

        fn multi_scalar_mul(
            &self,
            _points: &[FieldElement],
            _scalars_lo: &[FieldElement],
            _scalars_hi: &[FieldElement],
        ) -> Result<(FieldElement, FieldElement, FieldElement), BlackBoxResolutionError> {
            Ok((4_u128.into(), 5_u128.into(), 0_u128.into()))
        }

        fn ec_add(
            &self,
            _input1_x: &FieldElement,
            _input1_y: &FieldElement,
            _input1_infinite: &FieldElement,
            _input2_x: &FieldElement,
            _input2_y: &FieldElement,
            _input2_infinite: &FieldElement,
        ) -> Result<(FieldElement, FieldElement, FieldElement), BlackBoxResolutionError> {
            panic!("Path not trodden by this test")
        }

        fn poseidon2_permutation(
            &self,
            _inputs: &[FieldElement],
            _len: u32,
        ) -> Result<Vec<FieldElement>, BlackBoxResolutionError> {
            Ok(vec![0_u128.into(), 1_u128.into(), 2_u128.into(), 3_u128.into()])
        }
    }

    pub(crate) fn create_context(id: FunctionId) -> BrilligContext<FieldElement, Stack> {
        let options = BrilligOptions {
            enable_debug_trace: true,
            enable_debug_assertions: true,
            enable_array_copy_counter: false,
        };
        let mut context = BrilligContext::new(&options);
        context.enter_context(Label::function(id));
        context
    }

    pub(crate) fn create_entry_point_bytecode(
        context: BrilligContext<FieldElement, Stack>,
        arguments: Vec<BrilligParameter>,
        returns: Vec<BrilligParameter>,
    ) -> GeneratedBrillig<FieldElement> {
        let options = BrilligOptions {
            enable_debug_trace: false,
            enable_debug_assertions: context.enable_debug_assertions,
            enable_array_copy_counter: context.count_arrays_copied,
        };
        let artifact = context.artifact();
        let mut entry_point_artifact = BrilligContext::new_entry_point_artifact(
            arguments,
            returns,
            FunctionId::test_new(0),
            false,
            0,
            &options,
        );
        entry_point_artifact.link_with(&artifact);
        while let Some(unresolved_fn_label) = entry_point_artifact.first_unresolved_function_call()
        {
            let LabelType::Procedure(procedure_id) = unresolved_fn_label.label_type else {
                panic!("Test functions cannot be linked with other functions");
            };
            let procedure_artifact = compile_procedure(procedure_id, &options);
            entry_point_artifact.link_with(&procedure_artifact);
        }
        entry_point_artifact.finish()
    }

    pub(crate) fn create_and_run_vm(
        calldata: Vec<FieldElement>,
        bytecode: &[BrilligOpcode<FieldElement>],
    ) -> (VM<'_, FieldElement, DummyBlackBoxSolver>, usize, usize) {
        let profiling_active = false;
        let mut vm = VM::new(calldata, bytecode, &DummyBlackBoxSolver, profiling_active, None);

        let status = vm.process_opcodes();
        if let VMStatus::Finished { return_data_offset, return_data_size } = status {
            (vm, return_data_offset, return_data_size)
        } else {
            panic!("VM did not finish")
        }
    }

    /// Test a Brillig foreign call returning a vector
    #[test]
    fn test_brillig_ir_foreign_call_return_vector() {
        // pseudo-noir:
        //
        // #[oracle(get_number_sequence)]
        // unconstrained fn get_number_sequence(size: u32) -> Vec<u32> {
        // }
        //
        // unconstrained fn main() -> Vec<u32> {
        //   let the_sequence = get_number_sequence(12);
        //   assert(the_sequence.len() == 12);
        // }
        let options = BrilligOptions {
            enable_debug_trace: true,
            enable_debug_assertions: true,
            enable_array_copy_counter: false,
        };
        let mut context = BrilligContext::new(&options);
        let r_stack = ReservedRegisters::free_memory_pointer();
        // Start stack pointer at 0
        context.usize_const_instruction(r_stack, FieldElement::from(ReservedRegisters::len() + 3));
        let r_input_size = MemoryAddress::direct(ReservedRegisters::len());
        let r_array_ptr = MemoryAddress::direct(ReservedRegisters::len() + 1);
        let r_output_size = MemoryAddress::direct(ReservedRegisters::len() + 2);
        let r_equality = MemoryAddress::direct(ReservedRegisters::len() + 3);
        context.usize_const_instruction(r_input_size, FieldElement::from(12_usize));
        // copy our stack frame to r_array_ptr
        context.mov_instruction(r_array_ptr, r_stack);
        context.foreign_call_instruction(
            "make_number_sequence".into(),
            &[ValueOrArray::MemoryAddress(r_input_size)],
            &[HeapValueType::Simple(BitSize::Integer(IntegerBitSize::U32))],
            &[ValueOrArray::HeapVector(HeapVector { pointer: r_stack, size: r_output_size })],
            &[HeapValueType::Vector {
                value_types: vec![HeapValueType::Simple(BitSize::Integer(IntegerBitSize::U32))],
            }],
        );
        // push stack frame by r_returned_size
        context.memory_op_instruction(r_stack, r_output_size, r_stack, BrilligBinaryOp::Add);
        // check r_input_size == r_output_size
        context.memory_op_instruction(
            r_input_size,
            r_output_size,
            r_equality,
            BrilligBinaryOp::Equals,
        );
        // We push a JumpIf and Trap opcode directly as the constrain instruction
        // uses unresolved jumps which requires a block to be constructed in SSA and
        // we don't need this for Brillig IR tests
        context.push_opcode(BrilligOpcode::Const {
            destination: MemoryAddress::direct(0),
            bit_size: BitSize::Integer(IntegerBitSize::U32),
            value: FieldElement::from(0u64),
        });
        context.push_opcode(BrilligOpcode::JumpIf { condition: r_equality, location: 9 });
        context.push_opcode(BrilligOpcode::Trap {
            revert_data: HeapVector {
                pointer: MemoryAddress::direct(0),
                size: MemoryAddress::direct(0),
            },
        });

        context.stop_instruction(HeapVector {
            pointer: MemoryAddress::direct(0),
            size: MemoryAddress::direct(0),
        });

        let bytecode: Vec<BrilligOpcode<FieldElement>> = context.artifact().finish().byte_code;

        let mut vm = VM::new(vec![], &bytecode, &DummyBlackBoxSolver, false, None);
        let status = vm.process_opcodes();
        assert_eq!(
            status,
            VMStatus::ForeignCallWait {
                function: "make_number_sequence".to_string(),
                inputs: vec![ForeignCallParam::Single(FieldElement::from(12u128))]
            }
        );

        let number_sequence: Vec<FieldElement> =
            (0_usize..12_usize).map(FieldElement::from).collect();
        let response = ForeignCallResult { values: vec![ForeignCallParam::Array(number_sequence)] };
        vm.resolve_foreign_call(response);

        let status = vm.process_opcodes();
        assert_eq!(status, VMStatus::Finished { return_data_offset: 0, return_data_size: 0 });
    }
}
