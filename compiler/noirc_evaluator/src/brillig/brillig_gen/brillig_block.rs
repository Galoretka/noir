//! Module containing Brillig-gen logic specific to an SSA function's basic blocks.
use crate::brillig::brillig_ir::artifact::Label;
use crate::brillig::brillig_ir::brillig_variable::{
    BrilligArray, BrilligVariable, BrilligVector, SingleAddrVariable, type_to_heap_value_type,
};

use crate::brillig::brillig_ir::registers::RegisterAllocator;
use crate::brillig::brillig_ir::{
    BRILLIG_MEMORY_ADDRESSING_BIT_SIZE, BrilligBinaryOp, BrilligContext, ReservedRegisters,
};
use crate::ssa::ir::instruction::{ArrayOffset, ConstrainError, Hint};
use crate::ssa::ir::{
    basic_block::BasicBlockId,
    dfg::DataFlowGraph,
    function::FunctionId,
    instruction::{
        Binary, BinaryOp, Endian, Instruction, InstructionId, Intrinsic, TerminatorInstruction,
    },
    types::{NumericType, Type},
    value::{Value, ValueId},
};
use acvm::acir::brillig::{MemoryAddress, ValueOrArray};
use acvm::{FieldElement, acir::AcirField};
use fxhash::{FxHashMap as HashMap, FxHashSet as HashSet};
use iter_extended::vecmap;
use noirc_errors::call_stack::{CallStackHelper, CallStackId};
use num_bigint::BigUint;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::brillig_black_box::convert_black_box_call;
use super::brillig_block_variables::{BlockVariables, allocate_value_with_type};
use super::brillig_fn::FunctionContext;
use super::brillig_globals::HoistedConstantsToBrilligGlobals;
use super::constant_allocation::InstructionLocation;

/// Context structure for compiling a [function block][crate::ssa::ir::basic_block::BasicBlock] into Brillig bytecode.
pub(crate) struct BrilligBlock<'block, Registers: RegisterAllocator> {
    /// Per-function context shared across all of a function's blocks
    pub(crate) function_context: &'block mut FunctionContext,
    /// The basic block that is being converted
    pub(crate) block_id: BasicBlockId,
    /// Context for creating brillig opcodes
    pub(crate) brillig_context: &'block mut BrilligContext<FieldElement, Registers>,
    /// Tracks the available variable during the codegen of the block
    pub(crate) variables: BlockVariables,
    /// For each instruction, the set of values that are not used anymore after it.
    pub(crate) last_uses: HashMap<InstructionId, HashSet<ValueId>>,

    /// Mapping of SSA [ValueId]s to their already instantiated values in the Brillig IR.
    pub(crate) globals: &'block HashMap<ValueId, BrilligVariable>,
    /// Pre-instantiated constants values shared across functions which have hoisted to the global memory space.
    pub(crate) hoisted_global_constants: &'block HoistedConstantsToBrilligGlobals,
    /// Status variable for whether we are generating Brillig bytecode for a function or globals.
    /// This is primarily used for gating local variable specific logic.
    /// For example, liveness analysis for globals is unnecessary (and adds complexity),
    /// and instead globals live throughout the entirety of the program.
    pub(crate) building_globals: bool,
}

impl<'block, Registers: RegisterAllocator> BrilligBlock<'block, Registers> {
    /// Converts an SSA Basic block into a sequence of Brillig opcodes
    ///
    /// This method contains the necessary initial variable and register setup for compiling
    /// an SSA block by accessing the pre-computed liveness context.
    pub(crate) fn compile(
        function_context: &'block mut FunctionContext,
        brillig_context: &'block mut BrilligContext<FieldElement, Registers>,
        block_id: BasicBlockId,
        dfg: &DataFlowGraph,
        call_stacks: &mut CallStackHelper,
        globals: &'block HashMap<ValueId, BrilligVariable>,
        hoisted_global_constants: &'block HoistedConstantsToBrilligGlobals,
    ) {
        let live_in = function_context.liveness.get_live_in(&block_id);

        let mut live_in_no_globals = HashSet::default();
        for value in live_in {
            if let Value::NumericConstant { constant, typ } = dfg[*value] {
                if hoisted_global_constants.contains_key(&(constant, typ)) {
                    continue;
                }
            }
            if !dfg.is_global(*value) {
                live_in_no_globals.insert(*value);
            }
        }

        let variables = BlockVariables::new(live_in_no_globals);

        brillig_context.set_allocated_registers(
            variables
                .get_available_variables(function_context)
                .into_iter()
                .map(|variable| variable.extract_register())
                .collect(),
        );
        let last_uses = function_context.liveness.get_last_uses(&block_id).clone();

        let mut brillig_block = BrilligBlock {
            function_context,
            block_id,
            brillig_context,
            variables,
            last_uses,
            globals,
            hoisted_global_constants,
            building_globals: false,
        };

        brillig_block.convert_block(dfg, call_stacks);
    }

    /// Converts SSA globals into Brillig global values.
    ///
    /// Global values can be:
    /// - Numeric constants
    /// - Instructions that compute global values
    /// - Pre-hoisted constants (shared across functions and stored in global memory)
    ///
    /// This method expects SSA globals to already be converted to a [DataFlowGraph]
    /// as to share codegen logic with standard SSA function blocks.
    ///
    /// This method also emits any necessary debugging initialization logic (e.g., allocating a counter used
    /// to track array copies).
    ///
    /// # Returns
    /// A map of hoisted (constant, type) pairs to their allocated Brillig variables,
    /// which are used to resolve references to these constants throughout Brillig lowering.
    ///
    /// # Panics
    /// - Globals graph contains values other than a [constant][Value::NumericConstant] or [instruction][Value::Instruction]
    pub(crate) fn compile_globals(
        &mut self,
        globals: &DataFlowGraph,
        used_globals: &HashSet<ValueId>,
        call_stacks: &mut CallStackHelper,
        hoisted_global_constants: &BTreeSet<(FieldElement, NumericType)>,
    ) -> HashMap<(FieldElement, NumericType), BrilligVariable> {
        // Using the end of the global memory space adds more complexity as we
        // have to account for possible register de-allocations as part of regular global compilation.
        // Thus, we want to allocate any reserved global slots first.
        //
        // If this flag is set, compile the array copy counter as a global
        if self.brillig_context.count_array_copies() {
            let new_variable = allocate_value_with_type(self.brillig_context, Type::unsigned(32));
            self.brillig_context
                .const_instruction(new_variable.extract_single_addr(), FieldElement::zero());
        }

        for (id, value) in globals.values_iter() {
            if !used_globals.contains(&id) {
                continue;
            }
            match value {
                Value::NumericConstant { .. } => {
                    self.convert_ssa_value(id, globals);
                }
                Value::Instruction { instruction, .. } => {
                    self.convert_ssa_instruction(*instruction, globals, call_stacks);
                }
                _ => {
                    panic!(
                        "Expected either an instruction or a numeric constant for a global value"
                    )
                }
            }
        }

        let mut new_hoisted_constants = HashMap::default();
        for (constant, typ) in hoisted_global_constants.iter().copied() {
            let new_variable = allocate_value_with_type(self.brillig_context, Type::Numeric(typ));
            self.brillig_context.const_instruction(new_variable.extract_single_addr(), constant);
            if new_hoisted_constants.insert((constant, typ), new_variable).is_some() {
                unreachable!("ICE: ({constant:?}, {typ:?}) was already in cache");
            }
        }

        new_hoisted_constants
    }

    /// Internal method for [BrilligBlock::compile] that actually kicks off the Brillig compilation process
    ///
    /// At this point any Brillig context, should be contained in [BrilligBlock] and this function should
    /// only need to accept external SSA and debugging structures.
    fn convert_block(&mut self, dfg: &DataFlowGraph, call_stacks: &mut CallStackHelper) {
        // Add a label for this block
        let block_label = self.create_block_label_for_current_function(self.block_id);
        self.brillig_context.enter_context(block_label);

        self.convert_block_params(dfg);

        let block = &dfg[self.block_id];

        // Convert all of the instructions into the block
        for instruction_id in block.instructions() {
            self.convert_ssa_instruction(*instruction_id, dfg, call_stacks);
        }

        // Process the block's terminator instruction
        let terminator_instruction =
            block.terminator().expect("block is expected to be constructed");

        if self.brillig_context.count_array_copies()
            && matches!(terminator_instruction, TerminatorInstruction::Return { .. })
            && self.function_context.is_entry_point
        {
            self.brillig_context.emit_println_of_array_copy_counter();
        }

        self.convert_ssa_terminator(terminator_instruction, dfg);
    }

    /// Creates a unique global label for a block.
    ///
    /// This uses the current functions's function ID and the block ID
    /// Making the assumption that the block ID passed in belongs to this
    /// function.
    fn create_block_label_for_current_function(&self, block_id: BasicBlockId) -> Label {
        Self::create_block_label(self.function_context.function_id(), block_id)
    }
    /// Creates a unique label for a block using the function Id and the block ID.
    ///
    /// We implicitly assume that the function ID and the block ID is enough
    /// for us to create a unique label across functions and blocks.
    ///
    /// This is so that during linking there are no duplicates or labels being overwritten.
    fn create_block_label(function_id: FunctionId, block_id: BasicBlockId) -> Label {
        Label::block(function_id, block_id)
    }

    /// Converts an SSA terminator instruction into the necessary opcodes.
    fn convert_ssa_terminator(
        &mut self,
        terminator_instruction: &TerminatorInstruction,
        dfg: &DataFlowGraph,
    ) {
        self.initialize_constants(
            &self
                .function_context
                .constant_allocation
                .allocated_at_location(self.block_id, InstructionLocation::Terminator),
            dfg,
        );
        match terminator_instruction {
            TerminatorInstruction::JmpIf {
                condition,
                then_destination,
                else_destination,
                call_stack: _,
            } => {
                let condition = self.convert_ssa_single_addr_value(*condition, dfg);
                self.brillig_context.jump_if_instruction(
                    condition.address,
                    self.create_block_label_for_current_function(*then_destination),
                );
                self.brillig_context.jump_instruction(
                    self.create_block_label_for_current_function(*else_destination),
                );
            }
            TerminatorInstruction::Jmp {
                destination: destination_block,
                arguments,
                call_stack: _,
            } => {
                let target_block = &dfg[*destination_block];
                for (src, dest) in arguments.iter().zip(target_block.parameters()) {
                    // Destinations are block parameters so they should have been allocated previously.
                    let destination = self.variables.get_allocation(self.function_context, *dest);
                    let source = self.convert_ssa_value(*src, dfg);
                    self.brillig_context
                        .mov_instruction(destination.extract_register(), source.extract_register());
                }
                self.brillig_context.jump_instruction(
                    self.create_block_label_for_current_function(*destination_block),
                );
            }
            TerminatorInstruction::Return { return_values, .. } => {
                let return_registers = vecmap(return_values, |value_id| {
                    self.convert_ssa_value(*value_id, dfg).extract_register()
                });
                self.brillig_context.codegen_return(&return_registers);
            }
            TerminatorInstruction::Unreachable { .. } => {
                // If we assume this is unreachable code then there's nothing to do here
            }
        }
    }

    /// Allocates the block parameters that the given block is defining
    fn convert_block_params(&mut self, dfg: &DataFlowGraph) {
        // We don't allocate the block parameters here, we allocate the parameters the block is defining.
        // Since predecessors to a block have to know where the parameters of the block are allocated to pass data to it,
        // the block parameters need to be defined/allocated before the given block. Variable liveness provides when the block parameters are defined.
        // For the entry block, the defined block params will be the params of the function + any extra params of blocks it's the immediate dominator of.
        for param_id in self.function_context.liveness.defined_block_params(&self.block_id) {
            let value = &dfg[param_id];
            let param_type = match value {
                Value::Param { typ, .. } => typ,
                _ => unreachable!("ICE: Only Param type values should appear in block parameters"),
            };
            match param_type {
                // Simple parameters and arrays are passed as already filled registers
                // In the case of arrays, the values should already be in memory and the register should
                // Be a valid pointer to the array.
                // For slices, two registers are passed, the pointer to the data and a register holding the size of the slice.
                Type::Numeric(_) | Type::Array(..) | Type::Slice(..) | Type::Reference(_) => {
                    self.variables.define_variable(
                        self.function_context,
                        self.brillig_context,
                        param_id,
                        dfg,
                    );
                }
                Type::Function => todo!("ICE: Type::Function Param not supported"),
            }
        }
    }

    /// Converts an SSA instruction into a sequence of Brillig opcodes.
    fn convert_ssa_instruction(
        &mut self,
        instruction_id: InstructionId,
        dfg: &DataFlowGraph,
        call_stacks: &mut CallStackHelper,
    ) {
        let instruction = &dfg[instruction_id];
        let call_stack = dfg.get_instruction_call_stack(instruction_id);
        let call_stack_new_id = call_stacks.get_or_insert_locations(&call_stack);
        self.brillig_context.set_call_stack(call_stack_new_id);

        self.initialize_constants(
            &self.function_context.constant_allocation.allocated_at_location(
                self.block_id,
                InstructionLocation::Instruction(instruction_id),
            ),
            dfg,
        );
        match instruction {
            Instruction::Binary(binary) => {
                let result_var = self.variables.define_single_addr_variable(
                    self.function_context,
                    self.brillig_context,
                    dfg.instruction_results(instruction_id)[0],
                    dfg,
                );
                self.convert_ssa_binary(binary, dfg, result_var);
            }
            Instruction::Constrain(lhs, rhs, assert_message) => {
                let (condition, deallocate) = match (
                    dfg.get_numeric_constant_with_type(*lhs),
                    dfg.get_numeric_constant_with_type(*rhs),
                ) {
                    // If the constraint is of the form `x == u1 1` then we can simply constrain `x` directly
                    (Some((constant, NumericType::Unsigned { bit_size: 1 })), None)
                        if constant == FieldElement::one() =>
                    {
                        (self.convert_ssa_single_addr_value(*rhs, dfg), false)
                    }
                    (None, Some((constant, NumericType::Unsigned { bit_size: 1 })))
                        if constant == FieldElement::one() =>
                    {
                        (self.convert_ssa_single_addr_value(*lhs, dfg), false)
                    }

                    // Otherwise we need to perform the equality explicitly.
                    _ => {
                        let condition = SingleAddrVariable {
                            address: self.brillig_context.allocate_register(),
                            bit_size: 1,
                        };
                        self.convert_ssa_binary(
                            &Binary { lhs: *lhs, rhs: *rhs, operator: BinaryOp::Eq },
                            dfg,
                            condition,
                        );

                        (condition, true)
                    }
                };

                match assert_message {
                    Some(ConstrainError::Dynamic(selector, _, values)) => {
                        let payload_values =
                            vecmap(values, |value| self.convert_ssa_value(*value, dfg));
                        let payload_as_params = vecmap(values, |value| {
                            let value_type = dfg.type_of_value(*value);
                            FunctionContext::ssa_type_to_parameter(&value_type)
                        });
                        self.brillig_context.codegen_constrain_with_revert_data(
                            condition,
                            payload_values,
                            payload_as_params,
                            *selector,
                        );
                    }
                    Some(ConstrainError::StaticString(message)) => {
                        self.brillig_context.codegen_constrain(condition, Some(message.clone()));
                    }
                    None => {
                        self.brillig_context.codegen_constrain(condition, None);
                    }
                }
                if deallocate {
                    self.brillig_context.deallocate_single_addr(condition);
                }
            }
            Instruction::ConstrainNotEqual(..) => {
                unreachable!("only implemented in ACIR")
            }

            Instruction::Allocate => {
                let result_value = dfg.instruction_results(instruction_id)[0];
                let pointer = self.variables.define_single_addr_variable(
                    self.function_context,
                    self.brillig_context,
                    result_value,
                    dfg,
                );
                self.brillig_context.codegen_allocate_immediate_mem(pointer.address, 1);
            }
            Instruction::Store { address, value } => {
                let address_var = self.convert_ssa_single_addr_value(*address, dfg);
                let source_variable = self.convert_ssa_value(*value, dfg);

                self.brillig_context
                    .store_instruction(address_var.address, source_variable.extract_register());
            }
            Instruction::Load { address } => {
                let target_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    dfg.instruction_results(instruction_id)[0],
                    dfg,
                );

                let address_variable = self.convert_ssa_single_addr_value(*address, dfg);

                self.brillig_context
                    .load_instruction(target_variable.extract_register(), address_variable.address);
            }
            Instruction::Not(value) => {
                let condition_register = self.convert_ssa_single_addr_value(*value, dfg);
                let result_register = self.variables.define_single_addr_variable(
                    self.function_context,
                    self.brillig_context,
                    dfg.instruction_results(instruction_id)[0],
                    dfg,
                );
                self.brillig_context.not_instruction(condition_register, result_register);
            }
            Instruction::Call { func, arguments } => match &dfg[*func] {
                Value::ForeignFunction(func_name) => {
                    let result_ids = dfg.instruction_results(instruction_id);

                    let input_values = vecmap(arguments, |value_id| {
                        let variable = self.convert_ssa_value(*value_id, dfg);
                        self.brillig_context.variable_to_value_or_array(variable)
                    });
                    let input_value_types = vecmap(arguments, |value_id| {
                        let value_type = dfg.type_of_value(*value_id);
                        type_to_heap_value_type(&value_type)
                    });
                    let output_variables = vecmap(result_ids, |value_id| {
                        self.allocate_external_call_result(*value_id, dfg)
                    });
                    let output_values = vecmap(&output_variables, |variable| {
                        self.brillig_context.variable_to_value_or_array(*variable)
                    });
                    let output_value_types = vecmap(result_ids, |value_id| {
                        let value_type = dfg.type_of_value(*value_id);
                        type_to_heap_value_type(&value_type)
                    });
                    self.brillig_context.foreign_call_instruction(
                        func_name.to_owned(),
                        &input_values,
                        &input_value_types,
                        &output_values,
                        &output_value_types,
                    );

                    // Deallocate the temporary heap arrays and vectors of the inputs
                    for input_value in input_values {
                        match input_value {
                            ValueOrArray::HeapArray(array) => {
                                self.brillig_context.deallocate_heap_array(array);
                            }
                            ValueOrArray::HeapVector(vector) => {
                                self.brillig_context.deallocate_heap_vector(vector);
                            }
                            _ => {}
                        }
                    }

                    // Deallocate the temporary heap arrays and vectors of the outputs
                    for (i, (output_register, output_variable)) in
                        output_values.iter().zip(output_variables).enumerate()
                    {
                        match output_register {
                            // Returned vectors need to emit some bytecode to format the result as a BrilligVector
                            ValueOrArray::HeapVector(heap_vector) => {
                                self.brillig_context.initialize_externally_returned_vector(
                                    output_variable.extract_vector(),
                                    *heap_vector,
                                );
                                // Update the dynamic slice length maintained in SSA
                                if let ValueOrArray::MemoryAddress(len_index) = output_values[i - 1]
                                {
                                    let element_size = dfg[result_ids[i]].get_type().element_size();
                                    self.brillig_context
                                        .mov_instruction(len_index, heap_vector.size);
                                    self.brillig_context.codegen_usize_op_in_place(
                                        len_index,
                                        BrilligBinaryOp::UnsignedDiv,
                                        element_size,
                                    );
                                } else {
                                    unreachable!(
                                        "ICE: a vector must be preceded by a register containing its length"
                                    );
                                }
                                self.brillig_context.deallocate_heap_vector(*heap_vector);
                            }
                            ValueOrArray::HeapArray(array) => {
                                self.brillig_context.deallocate_heap_array(*array);
                            }
                            ValueOrArray::MemoryAddress(_) => {}
                        }
                    }
                }
                Value::Function(func_id) => {
                    let result_ids = dfg.instruction_results(instruction_id);
                    self.convert_ssa_function_call(*func_id, arguments, dfg, result_ids);
                }
                Value::Intrinsic(intrinsic) => {
                    // This match could be combined with the above but without it rust analyzer
                    // can't automatically insert any missing cases
                    match intrinsic {
                        Intrinsic::ArrayLen => {
                            let result_variable = self.variables.define_single_addr_variable(
                                self.function_context,
                                self.brillig_context,
                                dfg.instruction_results(instruction_id)[0],
                                dfg,
                            );
                            let param_id = arguments[0];
                            // Slices are represented as a tuple in the form: (length, slice contents).
                            // Thus, we can expect the first argument to a field in the case of a slice
                            // or an array in the case of an array.
                            if let Type::Numeric(_) = dfg.type_of_value(param_id) {
                                let len_variable = self.convert_ssa_value(arguments[0], dfg);
                                let length = len_variable.extract_single_addr();
                                self.brillig_context
                                    .mov_instruction(result_variable.address, length.address);
                            } else {
                                self.convert_ssa_array_len(
                                    arguments[0],
                                    result_variable.address,
                                    dfg,
                                );
                            }
                        }
                        Intrinsic::AsSlice => {
                            let source_variable = self.convert_ssa_value(arguments[0], dfg);
                            let result_ids = dfg.instruction_results(instruction_id);
                            let destination_len_variable =
                                self.variables.define_single_addr_variable(
                                    self.function_context,
                                    self.brillig_context,
                                    result_ids[0],
                                    dfg,
                                );
                            let destination_variable = self.variables.define_variable(
                                self.function_context,
                                self.brillig_context,
                                result_ids[1],
                                dfg,
                            );
                            let destination_vector = destination_variable.extract_vector();
                            let source_array = source_variable.extract_array();
                            let element_size = dfg.type_of_value(arguments[0]).element_size();

                            let source_size_register = self
                                .brillig_context
                                .make_usize_constant_instruction(source_array.size.into());

                            // we need to explicitly set the destination_len_variable
                            self.brillig_context.codegen_usize_op(
                                source_size_register.address,
                                destination_len_variable.address,
                                BrilligBinaryOp::UnsignedDiv,
                                element_size,
                            );

                            self.brillig_context.codegen_initialize_vector(
                                destination_vector,
                                source_size_register,
                                None,
                            );

                            // Items
                            let vector_items_pointer = self
                                .brillig_context
                                .codegen_make_vector_items_pointer(destination_vector);
                            let array_items_pointer =
                                self.brillig_context.codegen_make_array_items_pointer(source_array);

                            self.brillig_context.codegen_mem_copy(
                                array_items_pointer,
                                vector_items_pointer,
                                source_size_register,
                            );

                            self.brillig_context.deallocate_single_addr(source_size_register);
                            self.brillig_context.deallocate_register(vector_items_pointer);
                            self.brillig_context.deallocate_register(array_items_pointer);
                        }
                        Intrinsic::SlicePushBack
                        | Intrinsic::SlicePopBack
                        | Intrinsic::SlicePushFront
                        | Intrinsic::SlicePopFront
                        | Intrinsic::SliceInsert
                        | Intrinsic::SliceRemove => {
                            self.convert_ssa_slice_intrinsic_call(
                                dfg,
                                &dfg[*func],
                                instruction_id,
                                arguments,
                            );
                        }
                        Intrinsic::ToBits(endianness) => {
                            let results = dfg.instruction_results(instruction_id);

                            let source = self.convert_ssa_single_addr_value(arguments[0], dfg);

                            let target_array = self
                                .variables
                                .define_variable(
                                    self.function_context,
                                    self.brillig_context,
                                    results[0],
                                    dfg,
                                )
                                .extract_array();

                            let two = self
                                .brillig_context
                                .make_usize_constant_instruction(2_usize.into());

                            self.brillig_context.codegen_to_radix(
                                source,
                                target_array,
                                two,
                                matches!(endianness, Endian::Little),
                                true,
                            );

                            self.brillig_context.deallocate_single_addr(two);
                        }

                        Intrinsic::ToRadix(endianness) => {
                            let results = dfg.instruction_results(instruction_id);

                            let source = self.convert_ssa_single_addr_value(arguments[0], dfg);
                            let radix = self.convert_ssa_single_addr_value(arguments[1], dfg);

                            let target_array = self
                                .variables
                                .define_variable(
                                    self.function_context,
                                    self.brillig_context,
                                    results[0],
                                    dfg,
                                )
                                .extract_array();

                            self.brillig_context.codegen_to_radix(
                                source,
                                target_array,
                                radix,
                                matches!(endianness, Endian::Little),
                                false,
                            );
                        }
                        Intrinsic::Hint(Hint::BlackBox) => {
                            let result_ids = dfg.instruction_results(instruction_id);
                            self.convert_ssa_identity_call(arguments, dfg, result_ids);
                        }
                        Intrinsic::BlackBox(bb_func) => {
                            // Slices are represented as a tuple of (length, slice contents).
                            // We must check the inputs to determine if there are slices
                            // and make sure that we pass the correct inputs to the black box function call.
                            // The loop below only keeps the slice contents, so that
                            // setting up a black box function with slice inputs matches the expected
                            // number of arguments specified in the function signature.
                            let mut arguments_no_slice_len = Vec::new();
                            for (i, arg) in arguments.iter().enumerate() {
                                if matches!(dfg.type_of_value(*arg), Type::Numeric(_)) {
                                    if i < arguments.len() - 1 {
                                        if !matches!(
                                            dfg.type_of_value(arguments[i + 1]),
                                            Type::Slice(_)
                                        ) {
                                            arguments_no_slice_len.push(*arg);
                                        }
                                    } else {
                                        arguments_no_slice_len.push(*arg);
                                    }
                                } else {
                                    arguments_no_slice_len.push(*arg);
                                }
                            }

                            let function_arguments = vecmap(&arguments_no_slice_len, |arg| {
                                self.convert_ssa_value(*arg, dfg)
                            });
                            let function_results = dfg.instruction_results(instruction_id);
                            let function_results = vecmap(function_results, |result| {
                                self.allocate_external_call_result(*result, dfg)
                            });
                            convert_black_box_call(
                                self.brillig_context,
                                bb_func,
                                &function_arguments,
                                &function_results,
                            );
                        }
                        // `Intrinsic::AsWitness` is used to provide hints to acir-gen on optimal expression splitting.
                        // It is then useless in the brillig runtime and so we can ignore it
                        Intrinsic::AsWitness => (),
                        Intrinsic::FieldLessThan => {
                            let lhs = self.convert_ssa_single_addr_value(arguments[0], dfg);
                            assert!(lhs.bit_size == FieldElement::max_num_bits());
                            let rhs = self.convert_ssa_single_addr_value(arguments[1], dfg);
                            assert!(rhs.bit_size == FieldElement::max_num_bits());

                            let results = dfg.instruction_results(instruction_id);
                            let destination = self
                                .variables
                                .define_variable(
                                    self.function_context,
                                    self.brillig_context,
                                    results[0],
                                    dfg,
                                )
                                .extract_single_addr();
                            assert!(destination.bit_size == 1);

                            self.brillig_context.binary_instruction(
                                lhs,
                                rhs,
                                destination,
                                BrilligBinaryOp::LessThan,
                            );
                        }
                        Intrinsic::ArrayRefCount => {
                            let array = self.convert_ssa_value(arguments[0], dfg);
                            let result = dfg.instruction_results(instruction_id)[0];

                            let destination = self.variables.define_variable(
                                self.function_context,
                                self.brillig_context,
                                result,
                                dfg,
                            );
                            let destination = destination.extract_register();
                            let array = array.extract_register();
                            self.brillig_context.load_instruction(destination, array);
                        }
                        Intrinsic::SliceRefCount => {
                            let array = self.convert_ssa_value(arguments[1], dfg);
                            let result = dfg.instruction_results(instruction_id)[0];

                            let destination = self.variables.define_variable(
                                self.function_context,
                                self.brillig_context,
                                result,
                                dfg,
                            );
                            let destination = destination.extract_register();
                            let array = array.extract_register();
                            self.brillig_context.load_instruction(destination, array);
                        }
                        Intrinsic::IsUnconstrained
                        | Intrinsic::DerivePedersenGenerators
                        | Intrinsic::ApplyRangeConstraint
                        | Intrinsic::StrAsBytes
                        | Intrinsic::AssertConstant
                        | Intrinsic::StaticAssert
                        | Intrinsic::ArrayAsStrUnchecked => {
                            unreachable!("unsupported function call type {:?}", dfg[*func])
                        }
                    }
                }
                Value::Instruction { .. }
                | Value::Param { .. }
                | Value::NumericConstant { .. }
                | Value::Global(_) => {
                    unreachable!("unsupported function call type {:?}", dfg[*func])
                }
            },
            Instruction::Truncate { value, bit_size, .. } => {
                let result_ids = dfg.instruction_results(instruction_id);
                let destination_register = self.variables.define_single_addr_variable(
                    self.function_context,
                    self.brillig_context,
                    result_ids[0],
                    dfg,
                );
                let source_register = self.convert_ssa_single_addr_value(*value, dfg);
                self.brillig_context.codegen_truncate(
                    destination_register,
                    source_register,
                    *bit_size,
                );
            }
            Instruction::Cast(value, _) => {
                let result_ids = dfg.instruction_results(instruction_id);
                let destination_variable = self.variables.define_single_addr_variable(
                    self.function_context,
                    self.brillig_context,
                    result_ids[0],
                    dfg,
                );
                let source_variable = self.convert_ssa_single_addr_value(*value, dfg);
                self.convert_cast(destination_variable, source_variable);
            }
            Instruction::ArrayGet { array, index, offset } => {
                let result_ids = dfg.instruction_results(instruction_id);
                let destination_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    result_ids[0],
                    dfg,
                );

                let array_variable = self.convert_ssa_value(*array, dfg);
                let index_variable = self.convert_ssa_single_addr_value(*index, dfg);

                let has_offset = if dfg.is_constant(*index) {
                    // For constant indices it must be the case that they have been offset during SSA
                    assert!(*offset != ArrayOffset::None);
                    true
                } else {
                    false
                };

                self.convert_ssa_array_get(
                    array_variable,
                    index_variable,
                    destination_variable,
                    has_offset,
                );
            }
            Instruction::ArraySet { array, index, value, mutable, offset } => {
                let source_variable = self.convert_ssa_value(*array, dfg);
                let index_register = self.convert_ssa_single_addr_value(*index, dfg);
                let value_variable = self.convert_ssa_value(*value, dfg);

                let result_ids = dfg.instruction_results(instruction_id);
                let destination_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    result_ids[0],
                    dfg,
                );

                let has_offset = if dfg.is_constant(*index) {
                    // For constant indices it must be the case that they have been offset during SSA
                    assert!(*offset != ArrayOffset::None);
                    true
                } else {
                    false
                };

                self.convert_ssa_array_set(
                    source_variable,
                    destination_variable,
                    index_register,
                    value_variable,
                    *mutable,
                    has_offset,
                );
            }
            Instruction::RangeCheck { value, max_bit_size, assert_message } => {
                let value = self.convert_ssa_single_addr_value(*value, dfg);
                // SSA generates redundant range checks. A range check with a max bit size >= value.bit_size will always pass.
                if value.bit_size > *max_bit_size {
                    // Cast original value to field
                    let left = SingleAddrVariable {
                        address: self.brillig_context.allocate_register(),
                        bit_size: FieldElement::max_num_bits(),
                    };
                    self.convert_cast(left, value);

                    // Create a field constant with the max
                    let max = BigUint::from(2_u128).pow(*max_bit_size) - BigUint::from(1_u128);
                    let right = self.brillig_context.make_constant_instruction(
                        FieldElement::from_be_bytes_reduce(&max.to_bytes_be()),
                        FieldElement::max_num_bits(),
                    );

                    // Check if lte max
                    let condition =
                        SingleAddrVariable::new(self.brillig_context.allocate_register(), 1);
                    self.brillig_context.binary_instruction(
                        left,
                        right,
                        condition,
                        BrilligBinaryOp::LessThanEquals,
                    );

                    self.brillig_context.codegen_constrain(condition, assert_message.clone());
                    self.brillig_context.deallocate_single_addr(condition);
                    self.brillig_context.deallocate_single_addr(left);
                    self.brillig_context.deallocate_single_addr(right);
                }
            }
            Instruction::IncrementRc { value } => {
                let array_or_vector = self.convert_ssa_value(*value, dfg);
                let rc_register = self.brillig_context.allocate_register();

                // RC is always directly pointed by the array/vector pointer
                self.brillig_context
                    .load_instruction(rc_register, array_or_vector.extract_register());

                // Ensure we're not incrementing from 0 back to 1
                if self.brillig_context.enable_debug_assertions() {
                    self.assert_rc_neq_zero(rc_register);
                }

                self.brillig_context.codegen_usize_op_in_place(
                    rc_register,
                    BrilligBinaryOp::Add,
                    1,
                );
                self.brillig_context
                    .store_instruction(array_or_vector.extract_register(), rc_register);
                self.brillig_context.deallocate_register(rc_register);
            }
            Instruction::DecrementRc { value } => {
                let array_or_vector = self.convert_ssa_value(*value, dfg);
                let array_register = array_or_vector.extract_register();

                let rc_register = self.brillig_context.allocate_register();
                self.brillig_context.load_instruction(rc_register, array_register);

                // Check that the refcount isn't already 0 before we decrement. If we allow it to underflow
                // and become usize::MAX, and then return to 1, then it will indicate
                // an array as mutable when it probably shouldn't be.
                if self.brillig_context.enable_debug_assertions() {
                    self.assert_rc_neq_zero(rc_register);
                }

                self.brillig_context.codegen_usize_op_in_place(
                    rc_register,
                    BrilligBinaryOp::Sub,
                    1,
                );
                self.brillig_context.store_instruction(array_register, rc_register);
                self.brillig_context.deallocate_register(rc_register);
            }
            Instruction::EnableSideEffectsIf { .. } => {
                unreachable!("enable_side_effects not supported by brillig")
            }
            Instruction::IfElse { then_condition, then_value, else_condition: _, else_value } => {
                let then_condition = self.convert_ssa_single_addr_value(*then_condition, dfg);
                let then_value = self.convert_ssa_value(*then_value, dfg);
                let else_value = self.convert_ssa_value(*else_value, dfg);
                let result = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    dfg.instruction_results(instruction_id)[0],
                    dfg,
                );
                match (then_value, else_value) {
                    (
                        BrilligVariable::SingleAddr(then_address),
                        BrilligVariable::SingleAddr(else_address),
                    ) => {
                        self.brillig_context.conditional_move_instruction(
                            then_condition,
                            then_address,
                            else_address,
                            result.extract_single_addr(),
                        );
                    }
                    (
                        BrilligVariable::BrilligArray(then_array),
                        BrilligVariable::BrilligArray(else_array),
                    ) => {
                        // Pointer to the array which result from the if-else
                        let pointer = self.brillig_context.allocate_register();
                        self.brillig_context.conditional_move_instruction(
                            then_condition,
                            SingleAddrVariable::new_usize(then_array.pointer),
                            SingleAddrVariable::new_usize(else_array.pointer),
                            SingleAddrVariable::new_usize(pointer),
                        );
                        let if_else_array = BrilligArray { pointer, size: then_array.size };
                        // Copy the if-else array to the result
                        self.brillig_context
                            .call_array_copy_procedure(if_else_array, result.extract_array());
                    }
                    (
                        BrilligVariable::BrilligVector(then_vector),
                        BrilligVariable::BrilligVector(else_vector),
                    ) => {
                        // Pointer to the vector which result from the if-else
                        let pointer = self.brillig_context.allocate_register();
                        self.brillig_context.conditional_move_instruction(
                            then_condition,
                            SingleAddrVariable::new_usize(then_vector.pointer),
                            SingleAddrVariable::new_usize(else_vector.pointer),
                            SingleAddrVariable::new_usize(pointer),
                        );
                        let if_else_vector = BrilligVector { pointer };
                        // Copy the if-else vector to the result
                        self.brillig_context
                            .call_vector_copy_procedure(if_else_vector, result.extract_vector());
                    }
                    _ => unreachable!("ICE - then and else values must have the same type"),
                }
            }
            Instruction::MakeArray { elements: array, typ } => {
                let value_id = dfg.instruction_results(instruction_id)[0];
                if !self.variables.is_allocated(&value_id) {
                    let new_variable = self.variables.define_variable(
                        self.function_context,
                        self.brillig_context,
                        value_id,
                        dfg,
                    );

                    // Initialize the variable
                    match new_variable {
                        BrilligVariable::BrilligArray(brillig_array) => {
                            self.brillig_context.codegen_initialize_array(brillig_array);
                        }
                        BrilligVariable::BrilligVector(vector) => {
                            let size = self
                                .brillig_context
                                .make_usize_constant_instruction(array.len().into());
                            self.brillig_context.codegen_initialize_vector(vector, size, None);
                            self.brillig_context.deallocate_single_addr(size);
                        }
                        _ => unreachable!(
                            "ICE: Cannot initialize array value created as {new_variable:?}"
                        ),
                    };

                    // Write the items
                    let items_pointer = self
                        .brillig_context
                        .codegen_make_array_or_vector_items_pointer(new_variable);

                    self.initialize_constant_array(array, typ, dfg, items_pointer);

                    self.brillig_context.deallocate_register(items_pointer);
                }
            }
            Instruction::Noop => (),
        };

        if !self.building_globals {
            let dead_variables = self
                .last_uses
                .get(&instruction_id)
                .expect("Last uses for instruction should have been computed");

            for dead_variable in dead_variables {
                // Globals are reserved throughout the entirety of the program
                let not_hoisted_global = self.get_hoisted_global(dfg, *dead_variable).is_none();
                if !dfg.is_global(*dead_variable) && not_hoisted_global {
                    self.variables.remove_variable(
                        dead_variable,
                        self.function_context,
                        self.brillig_context,
                    );
                }
            }
        }
        self.brillig_context.set_call_stack(CallStackId::root());
    }

    /// Debug utility method to determine whether an array's reference count (RC) is zero.
    /// If RC's have drifted down to zero it means the RC increment/decrement instructions
    /// have been written incorrectly.
    ///
    /// Should only be called if [BrilligContext::enable_debug_assertions] returns true.
    fn assert_rc_neq_zero(&mut self, rc_register: MemoryAddress) {
        let zero = SingleAddrVariable::new(self.brillig_context.allocate_register(), 32);

        self.brillig_context.const_instruction(zero, FieldElement::zero());

        let condition = SingleAddrVariable::new(self.brillig_context.allocate_register(), 1);

        self.brillig_context.memory_op_instruction(
            zero.address,
            rc_register,
            condition.address,
            BrilligBinaryOp::Equals,
        );
        self.brillig_context.not_instruction(condition, condition);
        self.brillig_context
            .codegen_constrain(condition, Some("array ref-count underflow detected".to_owned()));
        self.brillig_context.deallocate_single_addr(condition);
    }

    /// Internal method to codegen an [Instruction::Call] to a [Value::Function]
    fn convert_ssa_function_call(
        &mut self,
        func_id: FunctionId,
        arguments: &[ValueId],
        dfg: &DataFlowGraph,
        result_ids: &[ValueId],
    ) {
        let argument_variables =
            vecmap(arguments, |argument_id| self.convert_ssa_value(*argument_id, dfg));
        let return_variables = vecmap(result_ids, |result_id| {
            self.variables.define_variable(
                self.function_context,
                self.brillig_context,
                *result_id,
                dfg,
            )
        });
        self.brillig_context.codegen_call(func_id, &argument_variables, &return_variables);
    }

    /// Copy the input arguments to the results.
    fn convert_ssa_identity_call(
        &mut self,
        arguments: &[ValueId],
        dfg: &DataFlowGraph,
        result_ids: &[ValueId],
    ) {
        let argument_variables =
            vecmap(arguments, |argument_id| self.convert_ssa_value(*argument_id, dfg));

        let return_variables = vecmap(result_ids, |result_id| {
            self.variables.define_variable(
                self.function_context,
                self.brillig_context,
                *result_id,
                dfg,
            )
        });

        for (src, dst) in argument_variables.into_iter().zip(return_variables) {
            self.brillig_context.mov_instruction(dst.extract_register(), src.extract_register());
        }
    }

    /// Load from an array variable at a specific index into a specified destination
    ///
    /// # Panics
    /// - The array variable is not a [BrilligVariable::BrilligArray] or [BrilligVariable::BrilligVector] when `has_offset` is false
    fn convert_ssa_array_get(
        &mut self,
        array_variable: BrilligVariable,
        index_variable: SingleAddrVariable,
        destination_variable: BrilligVariable,
        has_offset: bool,
    ) {
        let items_pointer = if has_offset {
            array_variable.extract_register()
        } else {
            self.brillig_context.codegen_make_array_or_vector_items_pointer(array_variable)
        };

        self.brillig_context.codegen_load_with_offset(
            items_pointer,
            index_variable,
            destination_variable.extract_register(),
        );

        if !has_offset {
            self.brillig_context.deallocate_register(items_pointer);
        }
    }

    /// Array set operation in SSA returns a new array or vector that is a copy of the parameter array or vector
    /// with a specific value changed.
    ///
    /// Whether an actual copy other the array occurs or we write into the same source array is determined by the
    /// [call into the array copy procedure][BrilligContext::call_array_copy_procedure].
    /// If the reference count of an array pointer is one, we write directly to the array.
    /// Look at the [procedure compilation][crate::brillig::brillig_ir::procedures::compile_procedure] for the exact procedure's codegen.
    fn convert_ssa_array_set(
        &mut self,
        source_variable: BrilligVariable,
        destination_variable: BrilligVariable,
        index_register: SingleAddrVariable,
        value_variable: BrilligVariable,
        mutable: bool,
        has_offset: bool,
    ) {
        assert!(index_register.bit_size == BRILLIG_MEMORY_ADDRESSING_BIT_SIZE);
        match (source_variable, destination_variable) {
            (
                BrilligVariable::BrilligArray(source_array),
                BrilligVariable::BrilligArray(destination_array),
            ) => {
                if !mutable {
                    self.brillig_context.call_array_copy_procedure(source_array, destination_array);
                }
            }
            (
                BrilligVariable::BrilligVector(source_vector),
                BrilligVariable::BrilligVector(destination_vector),
            ) => {
                if !mutable {
                    self.brillig_context
                        .call_vector_copy_procedure(source_vector, destination_vector);
                }
            }
            _ => unreachable!("ICE: array set on non-array"),
        }

        let destination_for_store = if mutable { source_variable } else { destination_variable };

        // Then set the value in the newly created array
        let items_pointer = if has_offset {
            destination_for_store.extract_register()
        } else {
            self.brillig_context.codegen_make_array_or_vector_items_pointer(destination_for_store)
        };

        self.brillig_context.codegen_store_with_offset(
            items_pointer,
            index_register,
            value_variable.extract_register(),
        );

        // If we mutated the source array we want instructions that use the destination array to point to the source array
        if mutable {
            self.brillig_context.mov_instruction(
                destination_variable.extract_register(),
                source_variable.extract_register(),
            );
        }

        if !has_offset {
            self.brillig_context.deallocate_register(items_pointer);
        }
    }

    /// Convert the SSA slice operations to brillig slice operations
    fn convert_ssa_slice_intrinsic_call(
        &mut self,
        dfg: &DataFlowGraph,
        intrinsic: &Value,
        instruction_id: InstructionId,
        arguments: &[ValueId],
    ) {
        let slice_id = arguments[1];
        let element_size = dfg.type_of_value(slice_id).element_size();
        let source_vector = self.convert_ssa_value(slice_id, dfg).extract_vector();

        let results = dfg.instruction_results(instruction_id);
        match intrinsic {
            Value::Intrinsic(Intrinsic::SlicePushBack) => {
                let target_len = match self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[0],
                    dfg,
                ) {
                    BrilligVariable::SingleAddr(register_index) => register_index,
                    _ => unreachable!("ICE: first value of a slice must be a register index"),
                };

                let target_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[1],
                    dfg,
                );

                let target_vector = target_variable.extract_vector();
                let item_values = vecmap(&arguments[2..element_size + 2], |arg| {
                    self.convert_ssa_value(*arg, dfg)
                });

                self.update_slice_length(
                    target_len.address,
                    arguments[0],
                    dfg,
                    BrilligBinaryOp::Add,
                );

                self.slice_push_back_operation(target_vector, source_vector, &item_values);
            }
            Value::Intrinsic(Intrinsic::SlicePushFront) => {
                let target_len = match self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[0],
                    dfg,
                ) {
                    BrilligVariable::SingleAddr(register_index) => register_index,
                    _ => unreachable!("ICE: first value of a slice must be a register index"),
                };

                let target_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[1],
                    dfg,
                );
                let target_vector = target_variable.extract_vector();
                let item_values = vecmap(&arguments[2..element_size + 2], |arg| {
                    self.convert_ssa_value(*arg, dfg)
                });

                self.update_slice_length(
                    target_len.address,
                    arguments[0],
                    dfg,
                    BrilligBinaryOp::Add,
                );

                self.slice_push_front_operation(target_vector, source_vector, &item_values);
            }
            Value::Intrinsic(Intrinsic::SlicePopBack) => {
                let target_len = match self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[0],
                    dfg,
                ) {
                    BrilligVariable::SingleAddr(register_index) => register_index,
                    _ => unreachable!("ICE: first value of a slice must be a register index"),
                };

                let target_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[1],
                    dfg,
                );

                let target_vector = target_variable.extract_vector();

                let pop_variables = vecmap(&results[2..element_size + 2], |result| {
                    self.variables.define_variable(
                        self.function_context,
                        self.brillig_context,
                        *result,
                        dfg,
                    )
                });

                self.update_slice_length(
                    target_len.address,
                    arguments[0],
                    dfg,
                    BrilligBinaryOp::Sub,
                );

                self.slice_pop_back_operation(target_vector, source_vector, &pop_variables);
            }
            Value::Intrinsic(Intrinsic::SlicePopFront) => {
                let target_len = match self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[element_size],
                    dfg,
                ) {
                    BrilligVariable::SingleAddr(register_index) => register_index,
                    _ => unreachable!("ICE: first value of a slice must be a register index"),
                };

                let pop_variables = vecmap(&results[0..element_size], |result| {
                    self.variables.define_variable(
                        self.function_context,
                        self.brillig_context,
                        *result,
                        dfg,
                    )
                });

                let target_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[element_size + 1],
                    dfg,
                );
                let target_vector = target_variable.extract_vector();

                self.update_slice_length(
                    target_len.address,
                    arguments[0],
                    dfg,
                    BrilligBinaryOp::Sub,
                );

                self.slice_pop_front_operation(target_vector, source_vector, &pop_variables);
            }
            Value::Intrinsic(Intrinsic::SliceInsert) => {
                let target_len = match self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[0],
                    dfg,
                ) {
                    BrilligVariable::SingleAddr(register_index) => register_index,
                    _ => unreachable!("ICE: first value of a slice must be a register index"),
                };

                let target_id = results[1];
                let target_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    target_id,
                    dfg,
                );

                let target_vector = target_variable.extract_vector();

                // Remove if indexing in insert is changed to flattened indexing
                // https://github.com/noir-lang/noir/issues/1889#issuecomment-1668048587
                let user_index = self.convert_ssa_single_addr_value(arguments[2], dfg);

                let converted_index =
                    self.brillig_context.make_usize_constant_instruction(element_size.into());

                self.brillig_context.memory_op_instruction(
                    converted_index.address,
                    user_index.address,
                    converted_index.address,
                    BrilligBinaryOp::Mul,
                );

                let items = vecmap(&arguments[3..element_size + 3], |arg| {
                    self.convert_ssa_value(*arg, dfg)
                });

                self.update_slice_length(
                    target_len.address,
                    arguments[0],
                    dfg,
                    BrilligBinaryOp::Add,
                );

                self.slice_insert_operation(target_vector, source_vector, converted_index, &items);
                self.brillig_context.deallocate_single_addr(converted_index);
            }
            Value::Intrinsic(Intrinsic::SliceRemove) => {
                let target_len = match self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    results[0],
                    dfg,
                ) {
                    BrilligVariable::SingleAddr(register_index) => register_index,
                    _ => unreachable!("ICE: first value of a slice must be a register index"),
                };

                let target_id = results[1];

                let target_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    target_id,
                    dfg,
                );
                let target_vector = target_variable.extract_vector();

                // Remove if indexing in remove is changed to flattened indexing
                // https://github.com/noir-lang/noir/issues/1889#issuecomment-1668048587
                let user_index = self.convert_ssa_single_addr_value(arguments[2], dfg);

                let converted_index =
                    self.brillig_context.make_usize_constant_instruction(element_size.into());
                self.brillig_context.memory_op_instruction(
                    converted_index.address,
                    user_index.address,
                    converted_index.address,
                    BrilligBinaryOp::Mul,
                );

                let removed_items = vecmap(&results[2..element_size + 2], |result| {
                    self.variables.define_variable(
                        self.function_context,
                        self.brillig_context,
                        *result,
                        dfg,
                    )
                });

                self.update_slice_length(
                    target_len.address,
                    arguments[0],
                    dfg,
                    BrilligBinaryOp::Sub,
                );

                self.slice_remove_operation(
                    target_vector,
                    source_vector,
                    converted_index,
                    &removed_items,
                );

                self.brillig_context.deallocate_single_addr(converted_index);
            }
            _ => unreachable!("ICE: Slice operation not supported"),
        }
    }

    /// Slices have a tuple structure (slice length, slice contents) to enable logic
    /// that uses dynamic slice lengths (such as with merging slices in the flattening pass).
    /// This method codegens an update to the slice length.
    ///
    /// The binary operation performed on the slice length is always an addition or subtraction of `1`.
    /// This is because the slice length holds the user length (length as displayed by a `.len()` call),
    /// and not a flattened length used internally to represent arrays of tuples.
    /// The length inside of `RegisterOrMemory::HeapVector` represents the entire flattened number
    /// of fields in the vector.
    ///
    /// Note that when we subtract a value, we expect that there is a constraint in SSA
    /// to check that the length isn't already 0. We could add a constraint opcode here,
    /// but if it's in SSA, there is a chance it can be optimized out.
    fn update_slice_length(
        &mut self,
        target_len: MemoryAddress,
        source_value: ValueId,
        dfg: &DataFlowGraph,
        binary_op: BrilligBinaryOp,
    ) {
        let source_len_variable = self.convert_ssa_value(source_value, dfg);
        let source_len = source_len_variable.extract_single_addr();

        self.brillig_context.codegen_usize_op(source_len.address, target_len, binary_op, 1);
    }

    /// Converts an SSA cast to a sequence of Brillig opcodes.
    /// Casting is only necessary when shrinking the bit size of a numeric value.
    fn convert_cast(&mut self, destination: SingleAddrVariable, source: SingleAddrVariable) {
        // We assume that `source` is a valid `target_type` as it's expected that a truncate instruction was emitted
        // to ensure this is the case.

        self.brillig_context.cast_instruction(destination, source);
    }

    /// Converts the Binary instruction into a sequence of Brillig opcodes.
    fn convert_ssa_binary(
        &mut self,
        binary: &Binary,
        dfg: &DataFlowGraph,
        result_variable: SingleAddrVariable,
    ) {
        let binary_type = type_of_binary_operation(
            dfg[binary.lhs].get_type().as_ref(),
            dfg[binary.rhs].get_type().as_ref(),
            binary.operator,
        );

        let left = self.convert_ssa_single_addr_value(binary.lhs, dfg);
        let right = self.convert_ssa_single_addr_value(binary.rhs, dfg);

        let (is_field, is_signed) = match binary_type {
            Type::Numeric(numeric_type) => match numeric_type {
                NumericType::Signed { .. } => (false, true),
                NumericType::Unsigned { .. } => (false, false),
                NumericType::NativeField => (true, false),
            },
            _ => unreachable!(
                "only numeric types are allowed in binary operations. References are handled separately"
            ),
        };

        let brillig_binary_op = match binary.operator {
            BinaryOp::Div => {
                if is_signed {
                    self.brillig_context.convert_signed_division(left, right, result_variable);
                    return;
                } else if is_field {
                    BrilligBinaryOp::FieldDiv
                } else {
                    BrilligBinaryOp::UnsignedDiv
                }
            }
            BinaryOp::Mod => {
                if is_signed {
                    self.convert_signed_modulo(left, right, result_variable);
                    return;
                } else {
                    BrilligBinaryOp::Modulo
                }
            }
            BinaryOp::Add { .. } => BrilligBinaryOp::Add,
            BinaryOp::Sub { .. } => BrilligBinaryOp::Sub,
            BinaryOp::Mul { .. } => BrilligBinaryOp::Mul,
            BinaryOp::Eq => BrilligBinaryOp::Equals,
            BinaryOp::Lt => {
                if is_signed {
                    self.convert_signed_less_than(left, right, result_variable);
                    return;
                } else {
                    BrilligBinaryOp::LessThan
                }
            }
            BinaryOp::And => BrilligBinaryOp::And,
            BinaryOp::Or => BrilligBinaryOp::Or,
            BinaryOp::Xor => BrilligBinaryOp::Xor,
            BinaryOp::Shl => BrilligBinaryOp::Shl,
            BinaryOp::Shr => {
                if is_signed {
                    self.convert_signed_shr(left, right, result_variable);
                    return;
                } else {
                    BrilligBinaryOp::Shr
                }
            }
        };

        self.brillig_context.binary_instruction(left, right, result_variable, brillig_binary_op);

        self.add_overflow_check(left, right, result_variable, binary, dfg, is_signed);
    }

    fn convert_signed_modulo(
        &mut self,
        left: SingleAddrVariable,
        right: SingleAddrVariable,
        result: SingleAddrVariable,
    ) {
        let scratch_var_i =
            SingleAddrVariable::new(self.brillig_context.allocate_register(), left.bit_size);
        let scratch_var_j =
            SingleAddrVariable::new(self.brillig_context.allocate_register(), left.bit_size);

        // i = left / right
        self.brillig_context.convert_signed_division(left, right, scratch_var_i);

        // j = i * right
        self.brillig_context.binary_instruction(
            scratch_var_i,
            right,
            scratch_var_j,
            BrilligBinaryOp::Mul,
        );

        // result_register = left - j
        self.brillig_context.binary_instruction(left, scratch_var_j, result, BrilligBinaryOp::Sub);
        // Free scratch registers
        self.brillig_context.deallocate_single_addr(scratch_var_i);
        self.brillig_context.deallocate_single_addr(scratch_var_j);
    }

    fn convert_signed_less_than(
        &mut self,
        left: SingleAddrVariable,
        right: SingleAddrVariable,
        result: SingleAddrVariable,
    ) {
        let biased_left =
            SingleAddrVariable::new(self.brillig_context.allocate_register(), left.bit_size);
        let biased_right =
            SingleAddrVariable::new(self.brillig_context.allocate_register(), right.bit_size);

        let bias = self
            .brillig_context
            .make_constant_instruction((1_u128 << (left.bit_size - 1)).into(), left.bit_size);

        self.brillig_context.binary_instruction(left, bias, biased_left, BrilligBinaryOp::Add);
        self.brillig_context.binary_instruction(right, bias, biased_right, BrilligBinaryOp::Add);

        self.brillig_context.binary_instruction(
            biased_left,
            biased_right,
            result,
            BrilligBinaryOp::LessThan,
        );

        self.brillig_context.deallocate_single_addr(biased_left);
        self.brillig_context.deallocate_single_addr(biased_right);
        self.brillig_context.deallocate_single_addr(bias);
    }

    fn convert_signed_shr(
        &mut self,
        left: SingleAddrVariable,
        right: SingleAddrVariable,
        result: SingleAddrVariable,
    ) {
        // Check if left is negative
        let left_is_negative = SingleAddrVariable::new(self.brillig_context.allocate_register(), 1);
        let max_positive = self
            .brillig_context
            .make_constant_instruction(((1_u128 << (left.bit_size - 1)) - 1).into(), left.bit_size);
        self.brillig_context.binary_instruction(
            max_positive,
            left,
            left_is_negative,
            BrilligBinaryOp::LessThan,
        );

        self.brillig_context.codegen_branch(left_is_negative.address, |ctx, is_negative| {
            if is_negative {
                // If right value is greater than the left bit size, return -1
                let rhs_does_not_overflow = SingleAddrVariable::new(ctx.allocate_register(), 1);
                let lhs_bit_size =
                    ctx.make_constant_instruction(left.bit_size.into(), right.bit_size);
                ctx.binary_instruction(
                    right,
                    lhs_bit_size,
                    rhs_does_not_overflow,
                    BrilligBinaryOp::LessThan,
                );

                ctx.codegen_branch(rhs_does_not_overflow.address, |ctx, no_overflow| {
                    if no_overflow {
                        let one = ctx.make_constant_instruction(1_u128.into(), left.bit_size);

                        // computes 2^right
                        let two = ctx.make_constant_instruction(2_u128.into(), left.bit_size);
                        let two_pow = ctx.make_constant_instruction(1_u128.into(), left.bit_size);
                        let right_u32 = SingleAddrVariable::new(ctx.allocate_register(), 32);
                        ctx.cast(right_u32, right);
                        let pow_body = |ctx: &mut BrilligContext<_, _>, _: SingleAddrVariable| {
                            ctx.binary_instruction(two_pow, two, two_pow, BrilligBinaryOp::Mul);
                        };
                        ctx.codegen_for_loop(None, right_u32.address, None, pow_body);

                        // Right shift using division on 1-complement
                        ctx.binary_instruction(left, one, result, BrilligBinaryOp::Add);
                        ctx.convert_signed_division(result, two_pow, result);
                        ctx.binary_instruction(result, one, result, BrilligBinaryOp::Sub);

                        // Clean-up
                        ctx.deallocate_single_addr(one);
                        ctx.deallocate_single_addr(two);
                        ctx.deallocate_single_addr(two_pow);
                        ctx.deallocate_single_addr(right_u32);
                    } else {
                        ctx.const_instruction(result, ((1_u128 << left.bit_size) - 1).into());
                    }
                });

                ctx.deallocate_single_addr(rhs_does_not_overflow);
            } else {
                ctx.binary_instruction(left, right, result, BrilligBinaryOp::Shr);
            }
        });

        self.brillig_context.deallocate_single_addr(left_is_negative);
    }

    /// Overflow checks for the following unsigned binary operations
    /// - Checked Add/Sub/Mul
    #[allow(clippy::too_many_arguments)]
    fn add_overflow_check(
        &mut self,
        left: SingleAddrVariable,
        right: SingleAddrVariable,
        result: SingleAddrVariable,
        binary: &Binary,
        dfg: &DataFlowGraph,
        is_signed: bool,
    ) {
        let bit_size = left.bit_size;

        if bit_size == FieldElement::max_num_bits() || is_signed {
            if is_signed
                && matches!(
                    binary.operator,
                    BinaryOp::Add { unchecked: false }
                        | BinaryOp::Sub { unchecked: false }
                        | BinaryOp::Mul { unchecked: false }
                )
            {
                panic!("Checked signed operations should all be removed before brillig-gen")
            }
            return;
        }

        match binary.operator {
            BinaryOp::Add { unchecked: false } => {
                let condition =
                    SingleAddrVariable::new(self.brillig_context.allocate_register(), 1);
                // Check that lhs <= result
                self.brillig_context.binary_instruction(
                    left,
                    result,
                    condition,
                    BrilligBinaryOp::LessThanEquals,
                );
                let msg = "attempt to add with overflow".to_string();
                self.brillig_context.codegen_constrain(condition, Some(msg));
                self.brillig_context.deallocate_single_addr(condition);
            }
            BinaryOp::Sub { unchecked: false } => {
                let condition =
                    SingleAddrVariable::new(self.brillig_context.allocate_register(), 1);
                // Check that rhs <= lhs
                self.brillig_context.binary_instruction(
                    right,
                    left,
                    condition,
                    BrilligBinaryOp::LessThanEquals,
                );
                let msg = "attempt to subtract with overflow".to_string();
                self.brillig_context.codegen_constrain(condition, Some(msg));
                self.brillig_context.deallocate_single_addr(condition);
            }
            BinaryOp::Mul { unchecked: false } => {
                let division_by_rhs_gives_lhs = |ctx: &mut BrilligContext<
                    FieldElement,
                    Registers,
                >| {
                    let condition = SingleAddrVariable::new(ctx.allocate_register(), 1);
                    let division = SingleAddrVariable::new(ctx.allocate_register(), bit_size);
                    // Check that result / rhs == lhs
                    ctx.binary_instruction(result, right, division, BrilligBinaryOp::UnsignedDiv);
                    ctx.binary_instruction(division, left, condition, BrilligBinaryOp::Equals);
                    let msg = "attempt to multiply with overflow".to_string();
                    ctx.codegen_constrain(condition, Some(msg));
                    ctx.deallocate_single_addr(condition);
                    ctx.deallocate_single_addr(division);
                };

                let rhs_may_be_zero =
                    dfg.get_numeric_constant(binary.rhs).is_none_or(|rhs| rhs.is_zero());
                if rhs_may_be_zero {
                    let is_right_zero =
                        SingleAddrVariable::new(self.brillig_context.allocate_register(), 1);
                    let zero =
                        self.brillig_context.make_constant_instruction(0_usize.into(), bit_size);
                    self.brillig_context.binary_instruction(
                        zero,
                        right,
                        is_right_zero,
                        BrilligBinaryOp::Equals,
                    );
                    self.brillig_context
                        .codegen_if_not(is_right_zero.address, division_by_rhs_gives_lhs);
                    self.brillig_context.deallocate_single_addr(is_right_zero);
                    self.brillig_context.deallocate_single_addr(zero);
                } else {
                    division_by_rhs_gives_lhs(self.brillig_context);
                }
            }
            _ => {}
        }
    }

    /// Accepts a list of constant values to be initialized
    ///
    /// This method does no checks as to whether the supplied constants are actually constants.
    /// It is expected that this method is called before converting an SSA instruction to Brillig
    /// and the constants to be initialized have been precomputed and stored in [FunctionContext::constant_allocation].
    fn initialize_constants(&mut self, constants: &[ValueId], dfg: &DataFlowGraph) {
        for &constant_id in constants {
            self.convert_ssa_value(constant_id, dfg);
        }
    }

    /// Converts an SSA [ValueId] into a [BrilligVariable]. Initializes if necessary.
    ///
    /// This method also first checks whether the SSA value is a hoisted global constant.
    /// If it has already been initialized in the global space, we return the already existing variable.
    /// If an SSA value is a [Value::Global], we check whether the value exists in the [BrilligBlock::globals] map,
    /// otherwise the method panics.
    pub(crate) fn convert_ssa_value(
        &mut self,
        value_id: ValueId,
        dfg: &DataFlowGraph,
    ) -> BrilligVariable {
        let value = &dfg[value_id];

        if let Some(variable) = self.get_hoisted_global(dfg, value_id) {
            return variable;
        }

        match value {
            Value::Global(_) => {
                unreachable!("Expected global value to be resolve to its inner value");
            }
            Value::Param { .. } | Value::Instruction { .. } => {
                // All block parameters and instruction results should have already been
                // converted to registers so we fetch from the cache.
                if dfg.is_global(value_id) {
                    *self.globals.get(&value_id).unwrap_or_else(|| {
                        panic!("ICE: Global value not found in cache {value_id}")
                    })
                } else {
                    self.variables.get_allocation(self.function_context, value_id)
                }
            }
            Value::NumericConstant { constant, .. } => {
                // Constants might have been converted previously or not, so we get or create and
                // (re)initialize the value inside.
                if self.variables.is_allocated(&value_id) {
                    self.variables.get_allocation(self.function_context, value_id)
                } else if dfg.is_global(value_id) {
                    *self.globals.get(&value_id).unwrap_or_else(|| {
                        panic!("ICE: Global value not found in cache {value_id}")
                    })
                } else {
                    let new_variable = self.variables.define_variable(
                        self.function_context,
                        self.brillig_context,
                        value_id,
                        dfg,
                    );

                    self.brillig_context
                        .const_instruction(new_variable.extract_single_addr(), *constant);
                    new_variable
                }
            }
            Value::Function(_) => {
                // For the debugger instrumentation we want to allow passing
                // around values representing function pointers, even though
                // there is no interaction with the function possible given that
                // value.
                let new_variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    value_id,
                    dfg,
                );

                self.brillig_context.const_instruction(
                    new_variable.extract_single_addr(),
                    value_id.to_u32().into(),
                );
                new_variable
            }
            Value::Intrinsic(_) | Value::ForeignFunction(_) => {
                todo!("ICE: Cannot convert value {value:?}")
            }
        }
    }

    /// Initializes a constant array in Brillig memory.
    ///
    /// This method is responsible for writing a constant array's contents into memory, starting
    /// from the given `pointer`. It chooses between compile-time or runtime initialization
    /// depending on the data pattern and size.
    ///
    /// If the array is large (`>10` items), its elements are all numeric, and all items are identical,
    /// a **runtime loop** is generated to perform the initialization more efficiently.
    ///
    /// Otherwise, the method falls back to a straightforward **compile-time** initialization, where
    /// each array element is emitted explicitly.
    ///
    /// This optimization helps reduce Brillig bytecode size and runtime cost when initializing large,
    /// uniform arrays.
    ///
    /// # Example
    /// For an array like [5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5], a runtime loop will be used
    /// For an array like [1, 2, 3, 4], each element will be set explicitly
    fn initialize_constant_array(
        &mut self,
        data: &im::Vector<ValueId>,
        typ: &Type,
        dfg: &DataFlowGraph,
        pointer: MemoryAddress,
    ) {
        if data.is_empty() {
            return;
        }
        let item_types = typ.clone().element_types();

        // Find out if we are repeating the same item over and over
        let first_item = data.iter().take(item_types.len()).copied().collect();
        let mut is_repeating = true;

        for item_index in (item_types.len()..data.len()).step_by(item_types.len()) {
            let item: Vec<_> = (0..item_types.len()).map(|i| data[item_index + i]).collect();
            if first_item != item {
                is_repeating = false;
                break;
            }
        }

        // If all the items are single address, and all have the same initial value, we can initialize the array in a runtime loop.
        // Since the cost in instructions for a runtime loop is in the order of magnitude of 10, we only do this if the item_count is bigger than that.
        let item_count = data.len() / item_types.len();

        if item_count > 10
            && is_repeating
            && item_types.iter().all(|typ| matches!(typ, Type::Numeric(_)))
        {
            self.initialize_constant_array_runtime(
                item_types, first_item, item_count, pointer, dfg,
            );
        } else {
            self.initialize_constant_array_comptime(data, dfg, pointer);
        }
    }

    /// Codegens Brillig instructions to initialize a large, constant array using a runtime loop.
    ///
    /// This method assumes the array consists of identical items repeated multiple times.
    /// It generates a Brillig loop that writes the repeated item into memory efficiently,
    /// reducing bytecode size and instruction count compared to unrolling each element.
    ///
    /// For complex types (e.g., tuples), multiple memory writes happen per loop iteration.
    /// For primitive type (e.g., u32, Field), a single memory write happens per loop iteration.
    fn initialize_constant_array_runtime(
        &mut self,
        item_types: Arc<Vec<Type>>,
        item_to_repeat: Vec<ValueId>,
        item_count: usize,
        pointer: MemoryAddress,
        dfg: &DataFlowGraph,
    ) {
        let mut subitem_to_repeat_variables = Vec::with_capacity(item_types.len());
        for subitem_id in item_to_repeat.into_iter() {
            subitem_to_repeat_variables.push(self.convert_ssa_value(subitem_id, dfg));
        }

        // Initialize loop bound with the array length
        let end_pointer_variable = self
            .brillig_context
            .make_usize_constant_instruction((item_count * item_types.len()).into());

        // Add the pointer to the array length
        self.brillig_context.memory_op_instruction(
            end_pointer_variable.address,
            pointer,
            end_pointer_variable.address,
            BrilligBinaryOp::Add,
        );

        // If this is an array with complex subitems, we need a custom step in the loop to write all the subitems while iterating.
        if item_types.len() > 1 {
            let step_variable =
                self.brillig_context.make_usize_constant_instruction(item_types.len().into());

            let subitem_pointer =
                SingleAddrVariable::new_usize(self.brillig_context.allocate_register());

            // Initializes a single subitem
            let initializer_fn =
                |ctx: &mut BrilligContext<_, _>, subitem_start_pointer: SingleAddrVariable| {
                    ctx.mov_instruction(subitem_pointer.address, subitem_start_pointer.address);
                    for (subitem_index, subitem) in
                        subitem_to_repeat_variables.into_iter().enumerate()
                    {
                        ctx.store_instruction(subitem_pointer.address, subitem.extract_register());
                        if subitem_index != item_types.len() - 1 {
                            ctx.memory_op_instruction(
                                subitem_pointer.address,
                                ReservedRegisters::usize_one(),
                                subitem_pointer.address,
                                BrilligBinaryOp::Add,
                            );
                        }
                    }
                };

            // for (let subitem_start_pointer = pointer; subitem_start_pointer < pointer + data_length; subitem_start_pointer += step) { initializer_fn(iterator) }
            self.brillig_context.codegen_for_loop(
                Some(pointer),
                end_pointer_variable.address,
                Some(step_variable.address),
                initializer_fn,
            );

            self.brillig_context.deallocate_single_addr(step_variable);
            self.brillig_context.deallocate_single_addr(subitem_pointer);
        } else {
            let subitem = subitem_to_repeat_variables.into_iter().next().unwrap();

            let initializer_fn =
                |ctx: &mut BrilligContext<_, _>, item_pointer: SingleAddrVariable| {
                    ctx.store_instruction(item_pointer.address, subitem.extract_register());
                };

            // for (let item_pointer = pointer; item_pointer < pointer + data_length; item_pointer += 1) { initializer_fn(iterator) }
            self.brillig_context.codegen_for_loop(
                Some(pointer),
                end_pointer_variable.address,
                None,
                initializer_fn,
            );
        }
        self.brillig_context.deallocate_single_addr(end_pointer_variable);
    }

    /// Codegens Brillig instructions to initialize a constant array at compile time.
    ///
    /// This method generates one `store` instruction per array element, writing each
    /// value from the SSA into consecutive memory addresses starting at `pointer`.
    ///
    /// Unlike [initialize_constant_array_runtime][Self::initialize_constant_array_runtime], this
    /// does not use loops and emits one instruction per write, which can increase bytecode size
    /// but provides fine-grained control.
    fn initialize_constant_array_comptime(
        &mut self,
        data: &im::Vector<crate::ssa::ir::map::Id<Value>>,
        dfg: &DataFlowGraph,
        pointer: MemoryAddress,
    ) {
        // Allocate a register for the iterator
        let write_pointer_register = self.brillig_context.allocate_register();

        self.brillig_context.mov_instruction(write_pointer_register, pointer);

        for (element_idx, element_id) in data.iter().enumerate() {
            let element_variable = self.convert_ssa_value(*element_id, dfg);
            // Store the item in memory
            self.brillig_context
                .store_instruction(write_pointer_register, element_variable.extract_register());

            if element_idx != data.len() - 1 {
                // Increment the write_pointer_register
                self.brillig_context.memory_op_instruction(
                    write_pointer_register,
                    ReservedRegisters::usize_one(),
                    write_pointer_register,
                    BrilligBinaryOp::Add,
                );
            }
        }

        self.brillig_context.deallocate_register(write_pointer_register);
    }

    /// Converts an SSA `ValueId` into a `MemoryAddress`. Initializes if necessary.
    fn convert_ssa_single_addr_value(
        &mut self,
        value_id: ValueId,
        dfg: &DataFlowGraph,
    ) -> SingleAddrVariable {
        let variable = self.convert_ssa_value(value_id, dfg);
        variable.extract_single_addr()
    }

    /// Allocates a variable to hold the result of an external function call (e.g., foreign or black box).
    /// For more information about foreign function calls in Brillig take a look at the [foreign call opcode][acvm::acir::brillig::Opcode::ForeignCall].
    ///
    /// This is typically used during Brillig codegen for calls to [Value::ForeignFunction], where
    /// external host functions return values back into the program.
    ///
    /// Numeric types and fixed-sized array results are directly allocated.
    /// As vector's are determined at runtime they are allocated differently.
    /// - Allocates memory for a [BrilligVector], which holds a pointer and dynamic size.
    /// - Initializes the pointer using the free memory pointer.
    /// - The actual size will be updated after the foreign function call returns.
    ///
    /// # Returns
    /// A [BrilligVariable] representing the allocated memory structure to store the foreign call's result.
    fn allocate_external_call_result(
        &mut self,
        result: ValueId,
        dfg: &DataFlowGraph,
    ) -> BrilligVariable {
        let typ = dfg[result].get_type();
        match typ.as_ref() {
            Type::Numeric(_) => self.variables.define_variable(
                self.function_context,
                self.brillig_context,
                result,
                dfg,
            ),

            Type::Array(..) => {
                let variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    result,
                    dfg,
                );
                let array = variable.extract_array();
                self.allocate_foreign_call_result_array(typ.as_ref(), array);

                variable
            }
            Type::Slice(_) => {
                let variable = self.variables.define_variable(
                    self.function_context,
                    self.brillig_context,
                    result,
                    dfg,
                );
                let vector = variable.extract_vector();

                // Set the pointer to the current stack frame
                // The stack pointer will then be updated by the caller of this method
                // once the external call is resolved and the array size is known
                self.brillig_context.load_free_memory_pointer_instruction(vector.pointer);

                variable
            }
            _ => {
                unreachable!("ICE: unsupported return type for black box call {typ:?}")
            }
        }
    }

    /// Recursively allocates memory for a nested array returned from a foreign function call.
    ///
    /// # Panics
    /// - If the provided `typ` is not an array.
    /// - If any slice types are encountered within the nested structure, since slices
    ///   require runtime size information and cannot be allocated statically here.
    fn allocate_foreign_call_result_array(&mut self, typ: &Type, array: BrilligArray) {
        let Type::Array(types, size) = typ else {
            unreachable!("ICE: allocate_foreign_call_array() expects an array, got {typ:?}")
        };

        self.brillig_context.codegen_initialize_array(array);

        let mut index = 0_usize;
        for _ in 0..*size {
            for element_type in types.iter() {
                match element_type {
                    Type::Array(_, nested_size) => {
                        let inner_array = BrilligArray {
                            pointer: self.brillig_context.allocate_register(),
                            size: *nested_size as usize,
                        };
                        self.allocate_foreign_call_result_array(element_type, inner_array);

                        // We add one since array.pointer points to [RC, ...items]
                        let idx = self
                            .brillig_context
                            .make_usize_constant_instruction((index + 1).into());
                        self.brillig_context.codegen_store_with_offset(
                            array.pointer,
                            idx,
                            inner_array.pointer,
                        );

                        self.brillig_context.deallocate_single_addr(idx);
                        self.brillig_context.deallocate_register(inner_array.pointer);
                    }
                    Type::Slice(_) => unreachable!(
                        "ICE: unsupported slice type in allocate_nested_array(), expects an array or a numeric type"
                    ),
                    _ => (),
                }
                index += 1;
            }
        }
    }

    /// Gets the "user-facing" length of an array.
    /// An array of structs with two fields would be stored as an 2 * array.len() array/vector.
    /// So we divide the length by the number of subitems in an item to get the user-facing length.
    fn convert_ssa_array_len(
        &mut self,
        array_id: ValueId,
        result_register: MemoryAddress,
        dfg: &DataFlowGraph,
    ) {
        let array_variable = self.convert_ssa_value(array_id, dfg);
        let element_size = dfg.type_of_value(array_id).element_size();

        match array_variable {
            BrilligVariable::BrilligArray(BrilligArray { size, .. }) => {
                self.brillig_context
                    .usize_const_instruction(result_register, (size / element_size).into());
            }
            BrilligVariable::BrilligVector(vector) => {
                let size = self.brillig_context.codegen_make_vector_length(vector);

                self.brillig_context.codegen_usize_op(
                    size.address,
                    result_register,
                    BrilligBinaryOp::UnsignedDiv,
                    element_size,
                );

                self.brillig_context.deallocate_single_addr(size);
            }
            _ => {
                unreachable!("ICE: Cannot get length of {array_variable:?}")
            }
        }
    }

    /// If the supplied value is a numeric constant check whether it is exists within
    /// the precomputed [hoisted globals map][Self::hoisted_global_constants].
    /// If the variable exists as a hoisted global return that value, otherwise return `None`.
    fn get_hoisted_global(
        &self,
        dfg: &DataFlowGraph,
        value_id: ValueId,
    ) -> Option<BrilligVariable> {
        if let Value::NumericConstant { constant, typ } = &dfg[value_id] {
            if let Some(variable) = self.hoisted_global_constants.get(&(*constant, *typ)) {
                return Some(*variable);
            }
        }
        None
    }
}

/// Returns the type of the operation considering the types of the operands
pub(crate) fn type_of_binary_operation(lhs_type: &Type, rhs_type: &Type, op: BinaryOp) -> Type {
    match (lhs_type, rhs_type) {
        (_, Type::Function) | (Type::Function, _) => {
            unreachable!("Functions are invalid in binary operations")
        }
        (_, Type::Reference(_)) | (Type::Reference(_), _) => {
            unreachable!("References are invalid in binary operations")
        }
        (_, Type::Array(..)) | (Type::Array(..), _) => {
            unreachable!("Arrays are invalid in binary operations")
        }
        (_, Type::Slice(..)) | (Type::Slice(..), _) => {
            unreachable!("Arrays are invalid in binary operations")
        }
        // If both sides are numeric type, then we expect their types to be
        // the same.
        (Type::Numeric(lhs_type), Type::Numeric(rhs_type))
            if op != BinaryOp::Shl && op != BinaryOp::Shr =>
        {
            assert_eq!(
                lhs_type, rhs_type,
                "lhs and rhs types in a binary operation are always the same but got {lhs_type} and {rhs_type}"
            );
            Type::Numeric(*lhs_type)
        }
        _ => lhs_type.clone(),
    }
}
