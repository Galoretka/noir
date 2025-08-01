pub(crate) mod context;
mod program;
mod tests;
mod value;

use acvm::AcirField;
use noirc_errors::call_stack::CallStack;
use noirc_frontend::hir_def::expr::Constructor;
use noirc_frontend::token::FmtStrFragment;
pub use program::Ssa;

use context::{Loop, SharedContext};
use iter_extended::{try_vecmap, vecmap};
use noirc_errors::Location;
use noirc_frontend::ast::UnaryOp;
use noirc_frontend::hir_def::types::Type as HirType;
use noirc_frontend::monomorphization::ast::{self, Expression, MatchCase, Program, While};
use noirc_frontend::shared::Visibility;

use crate::{
    errors::RuntimeError,
    ssa::{function_builder::data_bus::DataBusBuilder, ir::instruction::Intrinsic},
};

use self::{
    context::FunctionContext,
    value::{Tree, Values},
};

use super::ir::basic_block::BasicBlockId;
use super::ir::dfg::GlobalsGraph;
use super::ir::instruction::{ArrayOffset, ErrorType};
use super::ir::types::NumericType;
use super::validation::validate_function;
use super::{
    function_builder::data_bus::DataBus,
    ir::{
        function::RuntimeType,
        instruction::{BinaryOp, ConstrainError, TerminatorInstruction},
        types::Type,
        value::ValueId,
    },
};

pub(crate) const SSA_WORD_SIZE: u32 = 32;

/// Generates SSA for the given monomorphized program.
///
/// This function will generate the SSA but does not perform any optimizations on it.
pub fn generate_ssa(program: Program) -> Result<Ssa, RuntimeError> {
    // see which parameter has call_data/return_data attribute
    let is_databus = DataBusBuilder::is_databus(&program.main_function_signature);

    let is_return_data = matches!(program.return_visibility(), Visibility::ReturnData);

    let return_location = program.return_location;
    let mut context = SharedContext::new(program);

    let globals_dfg = std::mem::take(&mut context.globals_context.dfg);
    let globals = GlobalsGraph::from_dfg(globals_dfg);

    let main_id = Program::main_id();
    let main = context.program.main();

    // Queue the main function for compilation
    context.get_or_queue_function(main_id);
    let main_runtime = if main.unconstrained {
        RuntimeType::Brillig(main.inline_type)
    } else {
        RuntimeType::Acir(main.inline_type)
    };
    let mut function_context =
        FunctionContext::new(main.name.clone(), &main.parameters, main_runtime, &context, globals);

    // Generate the call_data bus from the relevant parameters. We create it *before* processing the function body
    let call_data = function_context.builder.call_data_bus(is_databus);

    function_context.codegen_function_body(&main.body)?;

    let mut return_data = DataBusBuilder::new();
    if let Some(return_location) = return_location {
        let block = function_context.builder.current_block();
        if function_context.builder.current_function.dfg[block].terminator().is_some()
            && is_return_data
        {
            // initialize the return_data bus from the return values
            let return_data_values =
                match function_context.builder.current_function.dfg[block].unwrap_terminator() {
                    TerminatorInstruction::Return { return_values, .. } => return_values.to_owned(),
                    _ => unreachable!("ICE - expect return on the last block"),
                };

            return_data = function_context.builder.initialize_data_bus(
                &return_data_values,
                return_data,
                None,
            );
        }
        let return_call_stack = function_context
            .builder
            .current_function
            .dfg
            .call_stack_data
            .add_location_to_root(return_location);
        let return_instruction =
            function_context.builder.current_function.dfg[block].unwrap_terminator_mut();

        match return_instruction {
            TerminatorInstruction::Return { return_values, call_stack } => {
                *call_stack = return_call_stack;
                // replace the returned values with the return data array
                if let Some(return_data_bus) = return_data.databus {
                    return_values.clear();
                    return_values.push(return_data_bus);
                }
            }
            _ => unreachable!("ICE - expect return on the last block"),
        }
    }
    // we save the data bus inside the dfg
    function_context.builder.current_function.dfg.data_bus =
        DataBus::get_data_bus(call_data, return_data);

    // Main has now been compiled and any other functions referenced within have been added to the
    // function queue as they were found in codegen_ident. This queueing will happen each time a
    // previously-unseen function is found so we need now only continue popping from this queue
    // to generate SSA for each function used within the program.
    while let Some((src_function_id, dest_id)) = context.pop_next_function_in_queue() {
        let function = &context.program[src_function_id];
        function_context.new_function(dest_id, function);
        function_context.codegen_function_body(&function.body)?;
    }

    let ssa = function_context.builder.finish();
    validate_ssa(&ssa);

    Ok(ssa)
}

pub fn validate_ssa(ssa: &Ssa) {
    for function in ssa.functions.values() {
        validate_function(function, ssa);
    }
}

impl FunctionContext<'_> {
    /// Codegen a function's body and set its return value to that of its last parameter.
    /// For functions returning nothing, this will be an empty list.
    fn codegen_function_body(&mut self, body: &Expression) -> Result<(), RuntimeError> {
        let return_value = self.codegen_expression(body)?;
        let results = return_value.into_value_list(self);

        self.builder.terminate_with_return(results);
        Ok(())
    }

    fn codegen_expression(&mut self, expr: &Expression) -> Result<Values, RuntimeError> {
        match expr {
            Expression::Ident(ident) => Ok(self.codegen_ident(ident)),
            Expression::Literal(literal) => self.codegen_literal(literal),
            Expression::Block(block) => self.codegen_block(block),
            Expression::Unary(unary) => self.codegen_unary(unary),
            Expression::Binary(binary) => self.codegen_binary(binary),
            Expression::Index(index) => self.codegen_index(index),
            Expression::Cast(cast) => self.codegen_cast(cast),
            Expression::For(for_expr) => self.codegen_for(for_expr),
            Expression::Loop(block) => self.codegen_loop(block),
            Expression::While(while_) => self.codegen_while(while_),
            Expression::If(if_expr) => self.codegen_if(if_expr),
            Expression::Match(match_expr) => self.codegen_match(match_expr),
            Expression::Tuple(tuple) => self.codegen_tuple(tuple),
            Expression::ExtractTupleField(tuple, index) => {
                self.codegen_extract_tuple_field(tuple, *index)
            }
            Expression::Call(call) => self.codegen_call(call),
            Expression::Let(let_expr) => self.codegen_let(let_expr),
            Expression::Constrain(expr, location, assert_payload) => {
                self.codegen_constrain(expr, *location, assert_payload)
            }
            Expression::Assign(assign) => self.codegen_assign(assign),
            Expression::Semi(semi) => self.codegen_semi(semi),
            Expression::Break => self.codegen_break(),
            Expression::Continue => self.codegen_continue(),
            Expression::Clone(expr) => self.codegen_clone(expr),
            Expression::Drop(expr) => self.codegen_drop(expr),
        }
    }

    /// Codegen any non-tuple expression so that we can unwrap the Values
    /// tree to return a single value for use with most SSA instructions.
    fn codegen_non_tuple_expression(&mut self, expr: &Expression) -> Result<ValueId, RuntimeError> {
        Ok(self.codegen_expression(expr)?.into_leaf().eval(self))
    }

    /// Codegen a reference to an ident.
    /// The only difference between this and codegen_ident is that if the variable is mutable
    /// as in `let mut var = ...;` the `Value::Mutable` will be returned directly instead of
    /// being automatically loaded from. This is needed when taking the reference of a variable
    /// to reassign to it. Note that mutable references `let x = &mut ...;` do not require this
    /// since they are not automatically loaded from and must be explicitly dereferenced.
    fn codegen_ident_reference(&mut self, ident: &ast::Ident) -> Values {
        match &ident.definition {
            ast::Definition::Local(id) => self.lookup(*id),
            ast::Definition::Global(id) => self.lookup_global(*id),
            ast::Definition::Function(id) => self.get_or_queue_function(*id),
            ast::Definition::Oracle(name) => self.builder.import_foreign_function(name).into(),
            ast::Definition::Builtin(name) | ast::Definition::LowLevel(name) => {
                match self.builder.import_intrinsic(name) {
                    Some(builtin) => builtin.into(),
                    None => panic!("No builtin function named '{name}' found"),
                }
            }
        }
    }

    /// Codegen an identifier, automatically loading its value if it is mutable.
    fn codegen_ident(&mut self, ident: &ast::Ident) -> Values {
        self.codegen_ident_reference(ident).map(|value| value.eval(self).into())
    }

    fn codegen_literal(&mut self, literal: &ast::Literal) -> Result<Values, RuntimeError> {
        match literal {
            ast::Literal::Array(array) => {
                let elements = self.codegen_array_elements(&array.contents)?;

                let typ = Self::convert_type(&array.typ).flatten();
                Ok(match array.typ {
                    ast::Type::Array(_, _) => {
                        self.codegen_array_checked(elements, typ[0].clone())?
                    }
                    _ => unreachable!("ICE: unexpected array literal type, got {}", array.typ),
                })
            }
            ast::Literal::Slice(array) => {
                let elements = self.codegen_array_elements(&array.contents)?;

                let typ = Self::convert_type(&array.typ).flatten();
                Ok(match array.typ {
                    ast::Type::Slice(_) => {
                        let slice_length =
                            self.builder.length_constant(array.contents.len() as u128);
                        let slice_contents =
                            self.codegen_array_checked(elements, typ[1].clone())?;
                        Tree::Branch(vec![slice_length.into(), slice_contents])
                    }
                    _ => unreachable!("ICE: unexpected slice literal type, got {}", array.typ),
                })
            }
            ast::Literal::Integer(value, typ, location) => {
                self.builder.set_location(*location);
                let typ = Self::convert_non_tuple_type(typ).unwrap_numeric();
                self.checked_numeric_constant(*value, typ).map(Into::into)
            }
            ast::Literal::Bool(value) => {
                // Don't need to call checked_numeric_constant here since `value` can only be true or false
                Ok(self.builder.numeric_constant(*value as u128, NumericType::bool()).into())
            }
            ast::Literal::Str(string) => Ok(self.codegen_string(string)),
            ast::Literal::FmtStr(fragments, number_of_fields, fields) => {
                let mut string = String::new();
                for fragment in fragments {
                    match fragment {
                        FmtStrFragment::String(value) => {
                            // Escape curly braces in non-interpolations
                            let value = value.replace('{', "{{").replace('}', "}}");
                            string.push_str(&value);
                        }
                        FmtStrFragment::Interpolation(value, _) => {
                            string.push('{');
                            string.push_str(value);
                            string.push('}');
                        }
                    }
                }

                // A caller needs multiple pieces of information to make use of a format string
                // The message string, the number of fields to be formatted, and the fields themselves
                let string = self.codegen_string(&string);
                let field_count = self
                    .builder
                    .numeric_constant(*number_of_fields as u128, NumericType::NativeField);
                let fields = self.codegen_expression(fields)?;

                Ok(Tree::Branch(vec![string, field_count.into(), fields]))
            }
            ast::Literal::Unit => Ok(Self::unit_value()),
        }
    }

    fn codegen_array_elements(
        &mut self,
        elements: &[Expression],
    ) -> Result<Vec<Values>, RuntimeError> {
        try_vecmap(elements, |element| self.codegen_expression(element))
    }

    fn codegen_string(&mut self, string: &str) -> Values {
        let elements = vecmap(string.as_bytes(), |byte| {
            self.builder.numeric_constant(*byte as u128, NumericType::char()).into()
        });
        let typ = Self::convert_non_tuple_type(&ast::Type::String(elements.len() as u32));
        self.codegen_array(elements, typ)
    }

    // Codegen an array but make sure that we do not have a nested slice
    ///
    /// The bool aspect of each array element indicates whether the element is an array constant
    /// or not. If it is, we avoid incrementing the reference count because we consider the
    /// constant to be moved into this larger array constant.
    fn codegen_array_checked(
        &mut self,
        elements: Vec<Values>,
        typ: Type,
    ) -> Result<Values, RuntimeError> {
        if typ.is_nested_slice() {
            return Err(RuntimeError::NestedSlice { call_stack: self.builder.get_call_stack() });
        }
        Ok(self.codegen_array(elements, typ))
    }

    /// Codegen an array by allocating enough space for each element and inserting separate
    /// store instructions until each element is stored. The store instructions will be separated
    /// by add instructions to calculate the new offset address to store to next.
    ///
    /// In the case of arrays of structs, the structs are flattened such that each field will be
    /// stored next to the other fields in memory. So an array such as [(1, 2), (3, 4)] is
    /// stored the same as the array [1, 2, 3, 4].
    ///
    /// The bool aspect of each array element indicates whether the element is an array constant
    /// or not. If it is, we avoid incrementing the reference count because we consider the
    /// constant to be moved into this larger array constant.
    ///
    /// The value returned from this function is always that of the allocate instruction.
    fn codegen_array(&mut self, elements: Vec<Values>, typ: Type) -> Values {
        let mut array = im::Vector::new();

        for element in elements {
            element.for_each(|element| {
                let element = element.eval(self);
                array.push_back(element);
            });
        }

        self.builder.insert_make_array(array, typ).into()
    }

    fn codegen_block(&mut self, block: &[Expression]) -> Result<Values, RuntimeError> {
        let mut result = Self::unit_value();
        for expr in block {
            result = self.codegen_expression(expr)?;
        }
        Ok(result)
    }

    fn codegen_unary(&mut self, unary: &ast::Unary) -> Result<Values, RuntimeError> {
        match unary.operator {
            UnaryOp::Not => {
                let rhs = self.codegen_expression(&unary.rhs)?;
                let rhs = rhs.into_leaf().eval(self);
                Ok(self.builder.insert_not(rhs).into())
            }
            UnaryOp::Minus => {
                let rhs = self.codegen_expression(&unary.rhs)?;
                let rhs = rhs.into_leaf().eval(self);
                let typ = self.builder.type_of_value(rhs).unwrap_numeric();
                let zero = self.builder.numeric_constant(0u128, typ);
                Ok(self.insert_binary(
                    zero,
                    noirc_frontend::ast::BinaryOpKind::Subtract,
                    rhs,
                    unary.location,
                ))
            }
            UnaryOp::Reference { mutable: _ } => {
                let rhs = self.codegen_reference(&unary.rhs)?;
                // If skip is set then `rhs` is a member access expression which is already a reference
                if unary.skip {
                    return Ok(rhs);
                }
                Ok(rhs.map(|rhs| {
                    match rhs {
                        value::Value::Normal(value) => {
                            let rhs_type = self.builder.current_function.dfg.type_of_value(value);
                            let alloc = self.builder.insert_allocate(rhs_type);
                            self.builder.insert_store(alloc, value);
                            Tree::Leaf(value::Value::Normal(alloc))
                        }
                        // The `.into()` here converts the Value::Mutable into
                        // a Value::Normal so it is no longer automatically dereferenced.
                        value::Value::Mutable(reference, _) => reference.into(),
                    }
                }))
            }
            UnaryOp::Dereference { .. } => {
                let rhs = self.codegen_expression(&unary.rhs)?;
                Ok(self.dereference(&rhs, &unary.result_type))
            }
        }
    }

    fn dereference(&mut self, values: &Values, element_type: &ast::Type) -> Values {
        let element_types = Self::convert_type(element_type);
        values.map_both(element_types, |value, element_type| {
            let reference = value.eval(self);
            self.builder.insert_load(reference, element_type).into()
        })
    }

    fn codegen_reference(&mut self, expr: &Expression) -> Result<Values, RuntimeError> {
        match expr {
            Expression::Ident(ident) => Ok(self.codegen_ident_reference(ident)),
            Expression::ExtractTupleField(tuple, index) => {
                let tuple = self.codegen_reference(tuple)?;
                Ok(Self::get_field(tuple, *index))
            }
            other => self.codegen_expression(other),
        }
    }

    fn codegen_binary(&mut self, binary: &ast::Binary) -> Result<Values, RuntimeError> {
        let lhs = self.codegen_non_tuple_expression(&binary.lhs)?;
        let rhs = self.codegen_non_tuple_expression(&binary.rhs)?;
        Ok(self.insert_binary(lhs, binary.operator, rhs, binary.location))
    }

    fn codegen_index(&mut self, index: &ast::Index) -> Result<Values, RuntimeError> {
        let array_or_slice = self.codegen_expression(&index.collection)?.into_value_list(self);
        let index_value = self.codegen_non_tuple_expression(&index.index)?;
        // Slices are represented as a tuple in the form: (length, slice contents).
        // Thus, slices require two value ids for their representation.
        let (array, slice_length) = if array_or_slice.len() > 1 {
            (array_or_slice[1], Some(array_or_slice[0]))
        } else {
            (array_or_slice[0], None)
        };

        self.codegen_array_index(
            array,
            index_value,
            &index.element_type,
            index.location,
            slice_length,
        )
    }

    /// This is broken off from codegen_index so that it can also be
    /// used to codegen a LValue::Index.
    ///
    /// Set load_result to true to load from each relevant index of the array
    /// (it may be multiple in the case of tuples). Set it to false to instead
    /// return a reference to each element, for use with the store instruction.
    fn codegen_array_index(
        &mut self,
        array: ValueId,
        index: ValueId,
        element_type: &ast::Type,
        location: Location,
        length: Option<ValueId>,
    ) -> Result<Values, RuntimeError> {
        // base_index = index * type_size
        let index = self.make_array_index(index);
        let type_size = Self::convert_type(element_type).size_of_type();
        let type_size =
            self.builder.numeric_constant(type_size as u128, NumericType::length_type());

        let array_type = &self.builder.type_of_value(array);

        // Checks for index Out-of-bounds
        match array_type {
            Type::Array(_, len) => {
                // Out of bounds array accesses are guaranteed to fail in ACIR so this check is performed implicitly.
                // We then only need to inject it for brillig functions.
                let runtime = self.builder.current_function.runtime();
                if runtime.is_brillig() {
                    let len =
                        self.builder.numeric_constant(*len as u128, NumericType::length_type());
                    self.codegen_access_check(index, len);
                }
            }
            Type::Slice(_) => {
                // The slice length is dynamic however so we can't rely on it being equal to the underlying memory
                // block as we can do for array types. We then inject a access check for both ACIR and brillig.
                self.codegen_access_check(
                    index,
                    length.expect("ICE: a length must be supplied for checking index"),
                );
            }

            _ => unreachable!("must have array or slice but got {array_type}"),
        }

        // This shouldn't overflow because the initial index is within array bounds
        let base_index = self.builder.set_location(location).insert_binary(
            index,
            BinaryOp::Mul { unchecked: true },
            type_size,
        );

        let mut field_index = 0u128;
        Ok(Self::map_type(element_type, |typ| {
            let index = self.make_offset(base_index, field_index);
            field_index += 1;

            // Reference counting in brillig relies on us incrementing reference
            // counts when nested arrays/slices are constructed or indexed. This
            // has no effect in ACIR code.
            let offset = ArrayOffset::None;
            let result = self.builder.insert_array_get(array, index, offset, typ);
            result.into()
        }))
    }

    /// Prepare an array or slice access.
    /// Check that the index being used to access an array/slice element
    /// is less than the (potentially dynamic) array/slice length.
    fn codegen_access_check(&mut self, index: ValueId, length: ValueId) {
        let index = self.make_array_index(index);
        // We convert the length as an array index type for comparison
        let array_len = self.make_array_index(length);
        let assert_message = Some("Index out of bounds".to_owned());

        let array_len_constant = self
            .builder
            .current_function
            .dfg
            .get_numeric_constant(array_len)
            .and_then(|value| value.try_to_u32());

        // This optimization seems to cause regressions in brillig so we restrict it to ACIR.
        let runtime = self.builder.current_function.runtime();
        if runtime.is_acir() && array_len_constant.is_some_and(u32::is_power_of_two) {
            // If the array length is a power of two then we can make use of the range check opcode
            // to assert that the index fits in the relevant number of bits.
            let array_len_constant = array_len_constant.expect("array checked to be constant");
            let array_len_bits = array_len_constant.ilog2();
            debug_assert_eq!(2u32.pow(array_len_bits), array_len_constant);
            // TODO(https://github.com/noir-lang/noir/issues/9191): this cast results in better circuit generation.
            // There's an optimization here that we should find automatically.
            let index_as_field = self.builder.insert_cast(index, NumericType::NativeField);
            self.builder.insert_range_check(index_as_field, array_len_bits, assert_message);
        } else {
            // If it's not a power of two then we need to do an explicit inequality and constraint.
            let is_offset_out_of_bounds =
                self.builder.insert_binary(index, BinaryOp::Lt, array_len);
            let true_const = self.builder.numeric_constant(true, NumericType::bool());

            self.builder.insert_constrain(
                is_offset_out_of_bounds,
                true_const,
                assert_message.map(ConstrainError::from),
            );
        }
    }

    fn codegen_cast(&mut self, cast: &ast::Cast) -> Result<Values, RuntimeError> {
        let lhs = self.codegen_non_tuple_expression(&cast.lhs)?;
        let typ = Self::convert_non_tuple_type(&cast.r#type).unwrap_numeric();

        Ok(self.insert_safe_cast(lhs, typ, cast.location).into())
    }

    /// Codegens a for loop, creating three new blocks in the process.
    /// The return value of a for loop is always a unit literal.
    ///
    /// For example, the loop `for i in start .. end { body }` is codegen'd as:
    ///
    /// ```text
    ///   v0 = ... codegen start ...
    ///   v1 = ... codegen end ...
    ///   br loop_entry(v0)
    /// loop_entry(i: Field):
    ///   v2 = lt i v1
    ///   jmpif v2, then: loop_body, else: loop_end
    /// loop_body():
    ///   v3 = ... codegen body ...
    ///   v4 = add 1, i
    ///   br loop_entry(v4)
    /// loop_end():
    ///   ... This is the current insert point after codegen_for finishes ...
    /// ```
    fn codegen_for(&mut self, for_expr: &ast::For) -> Result<Values, RuntimeError> {
        self.builder.set_location(for_expr.start_range_location);
        let start_index = self.codegen_non_tuple_expression(&for_expr.start_range)?;

        self.builder.set_location(for_expr.end_range_location);
        let end_index = self.codegen_non_tuple_expression(&for_expr.end_range)?;

        let range_bound = |id| self.builder.current_function.dfg.get_integer_constant(id);

        if let (Some(start_constant), Some(end_constant)) =
            (range_bound(start_index), range_bound(end_index))
        {
            // If we can determine that the loop contains zero iterations then there's no need to codegen the loop.
            if start_constant >= end_constant {
                return Ok(Self::unit_value());
            }
        }

        let loop_entry = self.builder.insert_block();
        let loop_body = self.builder.insert_block();
        let loop_end = self.builder.insert_block();

        // this is the 'i' in `for i in start .. end { block }`
        let index_type = Self::convert_non_tuple_type(&for_expr.index_type);
        let loop_index = self.builder.add_block_parameter(loop_entry, index_type);

        // Remember the blocks and variable used in case there are break/continue instructions
        // within the loop which need to jump to them.
        self.enter_loop(Loop { loop_entry, loop_index: Some(loop_index), loop_end });

        // Set the location of the initial jmp instruction to the start range. This is the location
        // used to issue an error if the start range cannot be determined at compile-time.
        self.builder.set_location(for_expr.start_range_location);
        self.builder.terminate_with_jmp(loop_entry, vec![start_index]);

        // Compile the loop entry block
        self.builder.switch_to_block(loop_entry);

        // Set the location of the ending Lt instruction and the jmpif back-edge of the loop to the
        // end range. These are the instructions used to issue an error if the end of the range
        // cannot be determined at compile-time.
        self.builder.set_location(for_expr.end_range_location);
        let jump_condition = self.builder.insert_binary(loop_index, BinaryOp::Lt, end_index);
        self.builder.terminate_with_jmpif(jump_condition, loop_body, loop_end);

        // Compile the loop body
        self.builder.switch_to_block(loop_body);
        self.define(for_expr.index_variable, loop_index.into());

        let result = self.codegen_expression(&for_expr.block);
        self.codegen_unless_break_or_continue(result, |this, _| {
            let new_loop_index = this.make_offset(loop_index, 1);
            this.builder.terminate_with_jmp(loop_entry, vec![new_loop_index]);
        })?;

        // Finish by switching back to the end of the loop
        self.builder.switch_to_block(loop_end);
        self.exit_loop();
        Ok(Self::unit_value())
    }

    /// Codegens a loop, creating two new blocks in the process.
    /// The return value of a loop is always a unit literal.
    ///
    /// For example, the loop `loop { body }` is codegen'd as:
    ///
    /// ```text
    ///   br loop_body()
    /// loop_body():
    ///   v3 = ... codegen body ...
    ///   br loop_body()
    /// loop_end():
    ///   ... This is the current insert point after codegen_for finishes ...
    /// ```
    fn codegen_loop(&mut self, block: &Expression) -> Result<Values, RuntimeError> {
        let loop_body = self.builder.insert_block();
        let loop_end = self.builder.insert_block();

        self.enter_loop(Loop { loop_entry: loop_body, loop_index: None, loop_end });

        self.builder.terminate_with_jmp(loop_body, vec![]);

        // Compile the loop body
        self.builder.switch_to_block(loop_body);
        let result = self.codegen_expression(block);
        self.codegen_unless_break_or_continue(result, |this, _| {
            this.builder.terminate_with_jmp(loop_body, vec![]);
        })?;

        // Finish by switching to the end of the loop
        self.builder.switch_to_block(loop_end);
        self.exit_loop();
        Ok(Self::unit_value())
    }

    /// Codegens a while loop, creating three new blocks in the process.
    /// The return value of a while is always a unit literal.
    ///
    /// For example, the loop `while cond { body }` is codegen'd as:
    ///
    /// ```text
    ///   jmp while_entry()
    /// while_entry:
    ///   v0 = ... codegen cond ...
    ///   jmpif v0, then: while_body, else: while_end
    /// while_body():
    ///   v3 = ... codegen body ...
    ///   jmp while_entry()
    /// while_end():
    ///   ... This is the current insert point after codegen_while finishes ...
    /// ```
    fn codegen_while(&mut self, while_: &While) -> Result<Values, RuntimeError> {
        let while_entry = self.builder.insert_block();
        let while_body = self.builder.insert_block();
        let while_end = self.builder.insert_block();

        self.builder.terminate_with_jmp(while_entry, vec![]);

        // Codegen the entry (where the condition is)
        self.builder.switch_to_block(while_entry);
        let condition = self.codegen_non_tuple_expression(&while_.condition)?;
        self.builder.terminate_with_jmpif(condition, while_body, while_end);

        self.enter_loop(Loop { loop_entry: while_entry, loop_index: None, loop_end: while_end });

        // Codegen the body
        self.builder.switch_to_block(while_body);
        let result = self.codegen_expression(&while_.body);
        self.codegen_unless_break_or_continue(result, |this, _| {
            this.builder.terminate_with_jmp(while_entry, vec![]);
        })?;

        // Finish by switching to the end of the while
        self.builder.switch_to_block(while_end);
        self.exit_loop();
        Ok(Self::unit_value())
    }

    /// Codegens an if expression, handling the case of what to do if there is no 'else'.
    ///
    /// For example, the expression `if cond { a } else { b }` is codegen'd as:
    ///
    /// ```text
    ///   v0 = ... codegen cond ...
    ///   jmpif v0, then: then_block, else: else_block
    /// then_block():
    ///   v1 = ... codegen a ...
    ///   br end_if(v1)
    /// else_block():
    ///   v2 = ... codegen b ...
    ///   br end_if(v2)
    /// end_if(v3: ?):  // Type of v3 matches the type of a and b
    ///   ... This is the current insert point after codegen_if finishes ...
    /// ```
    ///
    /// As another example, the expression `if cond { a }` is codegen'd as:
    ///
    /// ```text
    ///   v0 = ... codegen cond ...
    ///   jmpif v0, then: then_block, else: end_if
    /// then_block:
    ///   v1 = ... codegen a ...
    ///   br end_if()
    /// end_if:  // No block parameter is needed. Without an else, the unit value is always returned.
    ///   ... This is the current insert point after codegen_if finishes ...
    /// ```
    fn codegen_if(&mut self, if_expr: &ast::If) -> Result<Values, RuntimeError> {
        let condition = self.codegen_non_tuple_expression(&if_expr.condition)?;
        if let Some(result) = self.try_codegen_constant_if(condition, if_expr) {
            return result;
        }

        let then_block = self.builder.insert_block();
        let else_block = self.builder.insert_block();

        self.builder.terminate_with_jmpif(condition, then_block, else_block);

        self.builder.switch_to_block(then_block);
        let then_result = self.codegen_expression(&if_expr.consequence);

        let mut result = Self::unit_value();

        if let Some(alternative) = &if_expr.alternative {
            let end_block = self.builder.insert_block();

            self.codegen_unless_break_or_continue(then_result, |this, then_value| {
                let then_values = then_value.into_value_list(this);
                this.builder.terminate_with_jmp(end_block, then_values);
            })?;

            self.builder.switch_to_block(else_block);
            let else_result = self.codegen_expression(alternative);
            self.codegen_unless_break_or_continue(else_result, |this, else_value| {
                let else_values = else_value.into_value_list(this);
                this.builder.terminate_with_jmp(end_block, else_values);
            })?;

            // Create block arguments for the end block as needed to branch to
            // with our then and else value.
            result = Self::map_type(&if_expr.typ, |typ| {
                self.builder.add_block_parameter(end_block, typ).into()
            });

            // Must also set the then block to jmp to the end now
            self.builder.switch_to_block(end_block);
        } else {
            // In the case we have no 'else', the 'else' block is actually the end block.
            self.builder.terminate_with_jmp(else_block, vec![]);
            self.builder.switch_to_block(else_block);
        }

        Ok(result)
    }

    /// If the condition is known, skip codegen for the then/else branch and only compile the
    /// relevant branch.
    fn try_codegen_constant_if(
        &mut self,
        condition: ValueId,
        if_expr: &ast::If,
    ) -> Option<Result<Values, RuntimeError>> {
        let condition = self.builder.current_function.dfg.get_numeric_constant(condition)?;

        Some(if condition.is_zero() {
            match if_expr.alternative.as_ref() {
                Some(alternative) => self.codegen_expression(alternative),
                None => Ok(Self::unit_value()),
            }
        } else {
            self.codegen_expression(&if_expr.consequence)
        })
    }

    fn codegen_match(&mut self, match_expr: &ast::Match) -> Result<Values, RuntimeError> {
        let variable = self.lookup(match_expr.variable_to_match.0);

        // Any matches with only a single case we don't need to check the tag at all.
        // Note that this includes all matches on struct / tuple values.
        if match_expr.cases.len() == 1 && match_expr.default_case.is_none() {
            return self.no_match(variable, &match_expr.cases[0]);
        }

        // From here on we can assume `variable` is an enum, int, or bool value (not a struct/tuple)
        let tag = self.enum_tag(&variable);
        let tag_type = self.builder.type_of_value(tag).unwrap_numeric();

        let make_end_block = |this: &mut Self| -> (BasicBlockId, Values) {
            let block = this.builder.insert_block();
            let results = Self::map_type(&match_expr.typ, |typ| {
                this.builder.add_block_parameter(block, typ).into()
            });
            (block, results)
        };

        let (end_block, end_results) = make_end_block(self);

        // Optimization: if there is no default case we can jump directly to the last case
        // when finished with the previous case instead of using a jmpif with an unreachable
        // else block.
        let last_case = if match_expr.default_case.is_some() {
            match_expr.cases.len()
        } else {
            match_expr.cases.len() - 1
        };

        let mut blocks_to_merge = Vec::with_capacity(last_case);

        for i in 0..last_case {
            let case = &match_expr.cases[i];
            let variant_tag = self.variant_index_value(&case.constructor, tag_type)?;
            let eq = self.builder.insert_binary(tag, BinaryOp::Eq, variant_tag);

            let case_block = self.builder.insert_block();
            let else_block = self.builder.insert_block();
            self.builder.terminate_with_jmpif(eq, case_block, else_block);

            self.builder.switch_to_block(case_block);
            self.bind_case_arguments(variable.clone(), case);
            let results = self.codegen_expression(&case.branch);
            self.codegen_unless_break_or_continue(results, |this, results| {
                let results = results.into_value_list(this);

                // Each branch will jump to a different end block for now. We have to merge them all
                // later since SSA doesn't support more than two blocks jumping to the same end block.
                let local_end_block = make_end_block(this);
                this.builder.terminate_with_jmp(local_end_block.0, results);
                blocks_to_merge.push(local_end_block);
            })?;

            self.builder.switch_to_block(else_block);
        }

        let (last_local_end_block, last_results) = make_end_block(self);
        blocks_to_merge.push((last_local_end_block, last_results));

        if let Some(branch) = &match_expr.default_case {
            let results = self.codegen_expression(branch);
            self.codegen_unless_break_or_continue(results, |this, results| {
                let results = results.into_value_list(this);
                this.builder.terminate_with_jmp(last_local_end_block, results);
            })?;
        } else {
            // If there is no default case, assume we saved the last case from the
            // last_case optimization above
            let case = match_expr.cases.last().unwrap();
            self.bind_case_arguments(variable, case);
            let results = self.codegen_expression(&case.branch);
            self.codegen_unless_break_or_continue(results, |this, results| {
                let results = results.into_value_list(this);
                this.builder.terminate_with_jmp(last_local_end_block, results);
            })?;
        }

        // Merge blocks as last-in first-out:
        //
        // local_end_block0-----------------------------------------\
        //                                                           end block
        //                                                          /
        // local_end_block1---------------------\                  /
        //                                       new merge block2-/
        // local_end_block2--\                  /
        //                    new merge block1-/
        // local_end_block3--/
        //
        // This is necessary since SSA panics during flattening if we immediately
        // try to jump directly to end block instead: https://github.com/noir-lang/noir/issues/7323.
        //
        // It'd also be more efficient to merge them tournament-bracket style but that
        // also leads to panics during flattening for similar reasons.
        while let Some((block, results)) = blocks_to_merge.pop() {
            self.builder.switch_to_block(block);

            if let Some((block2, results2)) = blocks_to_merge.pop() {
                // Merge two blocks in the queue together
                let (new_merge, new_merge_results) = make_end_block(self);
                blocks_to_merge.push((new_merge, new_merge_results));

                let results = results.into_value_list(self);
                self.builder.terminate_with_jmp(new_merge, results);

                self.builder.switch_to_block(block2);
                let results2 = results2.into_value_list(self);
                self.builder.terminate_with_jmp(new_merge, results2);
            } else {
                // Finally done, jump to the end
                let results = results.into_value_list(self);
                self.builder.terminate_with_jmp(end_block, results);
            }
        }

        self.builder.switch_to_block(end_block);
        Ok(end_results)
    }

    fn variant_index_value(
        &mut self,
        constructor: &Constructor,
        typ: NumericType,
    ) -> Result<ValueId, RuntimeError> {
        match constructor {
            Constructor::Int(value) => self.checked_numeric_constant(*value, typ),
            other => Ok(self.builder.numeric_constant(other.variant_index(), typ)),
        }
    }

    fn no_match(&mut self, variable: Values, case: &MatchCase) -> Result<Values, RuntimeError> {
        if !case.arguments.is_empty() {
            self.bind_case_arguments(variable, case);
        }
        self.codegen_expression(&case.branch)
    }

    /// Extracts the tag value from an enum. Assumes enums are represented as a tuple
    /// where the tag is always the first field of the tuple.
    ///
    /// If the enum is only a single Leaf value, this expects the enum to consist only of the tag value.
    fn enum_tag(&mut self, enum_value: &Values) -> ValueId {
        match enum_value {
            Tree::Branch(values) => self.enum_tag(&values[0]),
            Tree::Leaf(value) => value.clone().eval(self),
        }
    }

    /// Bind the given variable ids to each argument of the given enum, using the
    /// variant at the given variant index. Note that this function makes assumptions that the
    /// representation of an enum is:
    ///
    /// (
    ///   tag_value,
    ///   (field0_0, .. field0_N), // fields of variant 0,
    ///   (field1_0, .. field1_N), // fields of variant 1,
    ///   ..,
    ///   (fieldM_0, .. fieldM_N), // fields of variant N,
    /// )
    fn bind_case_arguments(&mut self, enum_value: Values, case: &MatchCase) {
        if !case.arguments.is_empty() {
            if case.constructor.is_enum() {
                self.bind_enum_case_arguments(enum_value, case);
            } else if case.constructor.is_tuple_or_struct() {
                self.bind_tuple_or_struct_case_arguments(enum_value, case);
            }
        }
    }

    fn bind_enum_case_arguments(&mut self, enum_value: Values, case: &MatchCase) {
        let Tree::Branch(mut variants) = enum_value else {
            unreachable!("Expected enum value to contain each variant");
        };

        let variant_index = case.constructor.variant_index();

        // variant_index + 1 to account for the extra tag value
        let Tree::Branch(variant) = variants.swap_remove(variant_index + 1) else {
            unreachable!("Expected enum variant to contain a tag and each variant's arguments");
        };

        assert_eq!(
            variant.len(),
            case.arguments.len(),
            "Expected enum variant to contain a value for each variant argument"
        );

        for (value, (arg, _)) in variant.into_iter().zip(&case.arguments) {
            self.define(*arg, value);
        }
    }

    fn bind_tuple_or_struct_case_arguments(&mut self, struct_value: Values, case: &MatchCase) {
        let Tree::Branch(fields) = struct_value else {
            unreachable!("Expected struct value to contain each field");
        };

        assert_eq!(
            fields.len(),
            case.arguments.len(),
            "Expected field length to match constructor argument count"
        );

        for (value, (arg, _)) in fields.into_iter().zip(&case.arguments) {
            self.define(*arg, value);
        }
    }

    fn codegen_tuple(&mut self, tuple: &[Expression]) -> Result<Values, RuntimeError> {
        Ok(Tree::Branch(try_vecmap(tuple, |expr| self.codegen_expression(expr))?))
    }

    fn codegen_extract_tuple_field(
        &mut self,
        tuple: &Expression,
        field_index: usize,
    ) -> Result<Values, RuntimeError> {
        let tuple = self.codegen_expression(tuple)?;
        Ok(Self::get_field(tuple, field_index))
    }

    /// Generate SSA for a function call. Note that calls to built-in functions
    /// and intrinsics are also represented by the function call instruction.
    fn codegen_call(&mut self, call: &ast::Call) -> Result<Values, RuntimeError> {
        let function = self.codegen_non_tuple_expression(&call.func)?;
        let mut arguments = Vec::with_capacity(call.arguments.len());

        for argument in &call.arguments {
            let mut values = self.codegen_expression(argument)?.into_value_list(self);
            arguments.append(&mut values);
        }

        // Don't need to increment array reference counts when passed in as arguments
        // since it is done within the function to each parameter already.

        self.codegen_intrinsic_call_checks(function, &arguments, call.location);
        Ok(self.insert_call(function, arguments, &call.return_type, call.location))
    }

    fn codegen_intrinsic_call_checks(
        &mut self,
        function: ValueId,
        arguments: &[ValueId],
        location: Location,
    ) {
        if let Some(intrinsic) =
            self.builder.set_location(location).get_intrinsic_from_value(function)
        {
            match intrinsic {
                Intrinsic::SliceInsert => {
                    let one = self.builder.length_constant(1u128);

                    // We add one here in the case of a slice insert as a slice insert at the length of the slice
                    // can be converted to a slice push back
                    // This is unchecked as the slice length could be u32::max
                    let len_plus_one = self.builder.insert_binary(
                        arguments[0],
                        BinaryOp::Add { unchecked: false },
                        one,
                    );

                    self.codegen_access_check(arguments[2], len_plus_one);
                }
                Intrinsic::SliceRemove => {
                    self.codegen_access_check(arguments[2], arguments[0]);
                }
                Intrinsic::SlicePopFront | Intrinsic::SlicePopBack => {
                    // If the slice was empty, ACIR would fail appropriately as we check memory block sizes,
                    // but for Brillig we need to lay down an explicit check. By doing this in the SSA we
                    // might be able to optimize this away later.
                    if self.builder.current_function.runtime().is_brillig() {
                        let zero = self
                            .builder
                            .numeric_constant(0u32, NumericType::Unsigned { bit_size: 32 });
                        self.codegen_access_check(zero, arguments[0]);
                    }
                }
                _ => {
                    // Do nothing as the other intrinsics do not require checks
                }
            }
        }
    }

    /// Generate SSA for the given variable.
    /// If the variable is immutable, no special handling is necessary and we can return the given
    /// ValueId directly. If it is mutable, we'll need to allocate space for the value and store
    /// the initial value before returning the allocate instruction.
    fn codegen_let(&mut self, let_expr: &ast::Let) -> Result<Values, RuntimeError> {
        let mut values = self.codegen_expression(&let_expr.expression)?;

        values = values.map(|value| {
            let value = value.eval(self);

            Tree::Leaf(if let_expr.mutable {
                self.new_mutable_variable(value)
            } else {
                value::Value::Normal(value)
            })
        });

        self.define(let_expr.id, values);
        Ok(Self::unit_value())
    }

    fn codegen_constrain(
        &mut self,
        expr: &Expression,
        location: Location,
        assert_payload: &Option<Box<(Expression, HirType)>>,
    ) -> Result<Values, RuntimeError> {
        let expr = self.codegen_non_tuple_expression(expr)?;
        let true_literal = self.builder.numeric_constant(true, NumericType::bool());

        // Set the location here for any errors that may occur when we codegen the assert message
        self.builder.set_location(location);

        let assert_payload = self.codegen_constrain_error(assert_payload)?;

        self.builder.insert_constrain(expr, true_literal, assert_payload);

        Ok(Self::unit_value())
    }

    // This method does not necessary codegen the full assert message expression, thus it does not
    // return a `Values` object. Instead we check the internals of the expression to make sure
    // we have an `Expression::Call` as expected. An `Instruction::Call` is then constructed but not
    // inserted to the SSA as we want that instruction to be atomic in SSA with a constrain instruction.
    fn codegen_constrain_error(
        &mut self,
        assert_message: &Option<Box<(Expression, HirType)>>,
    ) -> Result<Option<ConstrainError>, RuntimeError> {
        let Some(assert_message_payload) = assert_message else { return Ok(None) };
        let (assert_message_expression, assert_message_typ) = assert_message_payload.as_ref();

        if let Expression::Literal(ast::Literal::Str(static_string)) = assert_message_expression {
            Ok(Some(ConstrainError::StaticString(static_string.clone())))
        } else {
            let error_type = ErrorType::Dynamic(assert_message_typ.clone());
            let selector = error_type.selector();
            let values = self.codegen_expression(assert_message_expression)?.into_value_list(self);
            let is_string_type = matches!(assert_message_typ, HirType::String(_));
            // Record custom types in the builder, outside of SSA instructions
            // This is made to avoid having Hir types in the SSA code.
            if !is_string_type {
                self.builder.record_error_type(selector, assert_message_typ.clone());
            }

            Ok(Some(ConstrainError::Dynamic(selector, is_string_type, values)))
        }
    }

    fn codegen_assign(&mut self, assign: &ast::Assign) -> Result<Values, RuntimeError> {
        // Evaluate the rhs first - when we load the expression in the lvalue we want that
        // to reflect any mutations from evaluating the rhs.
        let rhs = self.codegen_expression(&assign.expression)?;
        let lhs = self.extract_current_value(&assign.lvalue)?;

        self.assign_new_value(lhs, rhs);

        Ok(Self::unit_value())
    }

    fn codegen_semi(&mut self, expr: &Expression) -> Result<Values, RuntimeError> {
        self.codegen_expression(expr)?;
        Ok(Self::unit_value())
    }

    fn codegen_break(&mut self) -> Result<Values, RuntimeError> {
        let loop_end = self.current_loop().loop_end;
        self.builder.terminate_with_jmp(loop_end, Vec::new());

        Err(RuntimeError::BreakOrContinue { call_stack: CallStack::default() })
    }

    fn codegen_continue(&mut self) -> Result<Values, RuntimeError> {
        let loop_ = self.current_loop();

        // Must remember to increment i before jumping
        if let Some(loop_index) = loop_.loop_index {
            let new_loop_index = self.make_offset(loop_index, 1);
            self.builder.terminate_with_jmp(loop_.loop_entry, vec![new_loop_index]);
        } else {
            self.builder.terminate_with_jmp(loop_.loop_entry, vec![]);
        }

        Err(RuntimeError::BreakOrContinue { call_stack: CallStack::default() })
    }

    /// Evaluate the given expression, increment the reference count of each array within,
    /// and return the evaluated expression.
    fn codegen_clone(&mut self, expr: &Expression) -> Result<Values, RuntimeError> {
        let values = self.codegen_expression(expr)?;
        Ok(values.map(|value| {
            let value = value.eval(self);
            self.builder.increment_array_reference_count(value);
            Values::from(value)
        }))
    }

    /// Evaluate the given expression, decrement the reference count of each array within,
    /// and return unit.
    fn codegen_drop(&mut self, expr: &Expression) -> Result<Values, RuntimeError> {
        let values = self.codegen_expression(expr)?;
        values.for_each(|value| {
            let value = value.eval(self);
            self.builder.decrement_array_reference_count(value);
        });
        Ok(Self::unit_value())
    }

    #[must_use = "do not forget to add `?` at the end of this function call"]
    fn codegen_unless_break_or_continue<T, F>(
        &mut self,
        result: Result<T, RuntimeError>,
        f: F,
    ) -> Result<(), RuntimeError>
    where
        F: FnOnce(&mut Self, T),
    {
        match result {
            Ok(value) => {
                f(self, value);
                Ok(())
            }
            Err(RuntimeError::BreakOrContinue { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }
}
