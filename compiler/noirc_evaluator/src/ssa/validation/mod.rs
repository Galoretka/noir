//! Validator that checks whether a function is well formed.
//!
//! It validates:
//!
//! SSA form
//!
//! - That the function contains exactly one return block.
//! - That every checked signed addition or subtraction instruction is
//!   followed by a corresponding truncate instruction with the expected bit sizes.
//!
//! Type checking
//! - Check that the input values of certain instructions matches that instruction's constraint
//!   At the moment, only [Instruction::Binary], [Instruction::ArrayGet], and [Instruction::ArraySet]
//!   are type checked.
use core::panic;

use acvm::{AcirField, FieldElement};
use fxhash::{FxHashMap as HashMap, FxHashSet as HashSet};

pub(crate) mod dynamic_array_indices;

use crate::ssa::{ir::instruction::TerminatorInstruction, ssa_gen::Ssa};

use super::ir::{
    function::Function,
    instruction::{Binary, BinaryOp, Instruction, InstructionId, Intrinsic},
    types::{NumericType, Type},
    value::{Value, ValueId},
};

/// Aside the function being validated, the validator maintains internal state
/// during instruction visitation to track patterns that span multiple instructions.
struct Validator<'f> {
    function: &'f Function,
    ssa: &'f Ssa,

    // State for valid Field to integer casts
    // Range checks are laid down in isolation and can make for safe casts
    // If they occurred before the value being cast to a smaller type
    // Stores: A set of (value being range constrained, the value's max bit size)
    range_checks: HashMap<ValueId, u32>,
}

impl<'f> Validator<'f> {
    fn new(function: &'f Function, ssa: &'f Ssa) -> Self {
        Self { function, ssa, range_checks: HashMap::default() }
    }

    /// Enforces that every cast from Field -> unsigned/signed integer must obey the following invariants:
    /// The value being cast is either:
    /// 1. A truncate instruction that ensures the cast is valid
    /// 2. A constant value known to be in-range
    /// 3. A division or other operation whose result is known to fit within the target bit size
    ///
    /// Our initial SSA gen only generates preceding truncates for safe casts.
    /// The cases accepted here are extended past what we perform during our initial SSA gen
    /// to mirror the instruction simplifier and other logic that could be accepted as a safe cast.
    fn validate_field_to_integer_cast_invariant(&mut self, instruction_id: InstructionId) {
        let dfg = &self.function.dfg;

        let (cast_input, typ) = match &dfg[instruction_id] {
            Instruction::Cast(cast_input, typ) => (*cast_input, *typ),
            Instruction::RangeCheck { value, max_bit_size, .. } => {
                self.range_checks.insert(*value, *max_bit_size);
                return;
            }
            _ => return,
        };

        if !matches!(dfg.type_of_value(cast_input), Type::Numeric(NumericType::NativeField)) {
            return;
        }

        let (NumericType::Signed { bit_size: target_type_size }
        | NumericType::Unsigned { bit_size: target_type_size }) = typ
        else {
            return;
        };

        // If the cast input has already been range constrained to a bit size that fits
        // in the destination type, we have a safe cast.
        if let Some(max_bit_size) = self.range_checks.get(&cast_input) {
            assert!(*max_bit_size <= target_type_size);
            return;
        }

        match &dfg[cast_input] {
            Value::Instruction { instruction, .. } => match &dfg[*instruction] {
                Instruction::Truncate { value: _, bit_size, max_bit_size } => {
                    assert!(*bit_size <= target_type_size);
                    assert!(*max_bit_size <= FieldElement::max_num_bits());
                }
                Instruction::Binary(Binary { lhs, rhs, operator: BinaryOp::Div, .. })
                    if dfg.is_constant(*rhs) =>
                {
                    let numerator_bits = dfg.type_of_value(*lhs).bit_size();
                    let divisor = dfg.get_numeric_constant(*rhs).unwrap();
                    let divisor_bits = divisor.num_bits();
                    let max_quotient_bits = numerator_bits - divisor_bits;

                    assert!(
                        max_quotient_bits <= target_type_size,
                        "Cast from field after div could exceed bit size: expected ≤ {target_type_size}, got {max_quotient_bits}"
                    );
                }
                _ => {
                    panic!("Invalid cast from Field, must be truncated or provably safe");
                }
            },
            Value::NumericConstant { constant, .. } => {
                let max_val_bits = constant.num_bits();
                assert!(
                    max_val_bits <= target_type_size,
                    "Constant too large for cast target: {max_val_bits} bits > {target_type_size}"
                );
            }
            _ => {
                panic!(
                    "Invalid cast from Field, not preceded by valid truncation or known safe value"
                );
            }
        }
    }

    // Validates there is exactly one return block
    fn validate_single_return_block(&self) {
        let reachable_blocks = self.function.reachable_blocks();

        let return_blocks: HashSet<_> = reachable_blocks
            .iter()
            .filter(|block| {
                let terminator = self.function.dfg[**block].terminator().unwrap_or_else(|| {
                    panic!("Function {} has no terminator in block {block}", self.function.id())
                });
                matches!(terminator, TerminatorInstruction::Return { .. })
            })
            .collect();

        if return_blocks.len() > 1 {
            panic!("Function {} has multiple return blocks {return_blocks:?}", self.function.id())
        }
    }

    /// Validates that the instruction has the expected types associated with the values in each instruction
    fn type_check_instruction(&self, instruction: InstructionId) {
        let dfg = &self.function.dfg;
        match &dfg[instruction] {
            Instruction::Binary(Binary { lhs, rhs, operator }) => {
                let lhs_type = dfg.type_of_value(*lhs);
                let rhs_type = dfg.type_of_value(*rhs);
                match operator {
                    BinaryOp::Lt => {
                        if lhs_type != rhs_type {
                            panic!(
                                "Left-hand side and right-hand side of `lt` must have the same type"
                            );
                        }

                        if matches!(lhs_type, Type::Numeric(NumericType::NativeField)) {
                            panic!("Cannot use `lt` with field elements");
                        }
                    }
                    BinaryOp::Shl => {
                        if !matches!(rhs_type, Type::Numeric(NumericType::Unsigned { bit_size: 8 }))
                        {
                            panic!("Right-hand side of `shl` must be u8");
                        }
                    }
                    BinaryOp::Shr => {
                        if !matches!(rhs_type, Type::Numeric(NumericType::Unsigned { bit_size: 8 }))
                        {
                            panic!("Right-hand side of `shr` must be u8");
                        }
                    }
                    _ => {
                        if lhs_type != rhs_type {
                            panic!(
                                "Left-hand side and right-hand side of `{operator}` must have the same type"
                            );
                        }
                    }
                }
            }
            Instruction::ArrayGet { index, .. } | Instruction::ArraySet { index, .. } => {
                let index_type = dfg.type_of_value(*index);
                if !matches!(index_type, Type::Numeric(NumericType::Unsigned { bit_size: 32 })) {
                    panic!("ArrayGet/ArraySet index must be u32");
                }
            }
            Instruction::Call { func, arguments } => {
                match &dfg[*func] {
                    Value::Intrinsic(intrinsic) => {
                        match intrinsic {
                            Intrinsic::ToRadix(_) => {
                                assert_eq!(arguments.len(), 2);

                                let value_typ = dfg.type_of_value(arguments[0]);
                                assert!(matches!(
                                    value_typ,
                                    Type::Numeric(NumericType::NativeField)
                                ));

                                let radix_typ = dfg.type_of_value(arguments[1]);
                                assert!(matches!(
                                    radix_typ,
                                    Type::Numeric(NumericType::Unsigned { bit_size: 32 })
                                ));
                            }
                            Intrinsic::ToBits(_) => {
                                // Intrinsic::ToBits always has a set radix
                                assert_eq!(arguments.len(), 1);
                                let value_typ = dfg.type_of_value(arguments[0]);
                                assert!(matches!(
                                    value_typ,
                                    Type::Numeric(NumericType::NativeField)
                                ));
                            }
                            _ => {}
                        }
                    }
                    Value::Function(func_id) => {
                        let called_function = &self.ssa.functions[func_id];
                        if let Some(returns) = called_function.returns() {
                            let instruction_results = dfg.instruction_results(instruction);
                            if instruction_results.len() != returns.len() {
                                panic!(
                                    "Function call to {} expected {} return values, but got {}",
                                    func_id,
                                    instruction_results.len(),
                                    returns.len(),
                                );
                            }
                            for (index, (instruction_result, return_value)) in
                                instruction_results.iter().zip(returns).enumerate()
                            {
                                let return_type = called_function.dfg.type_of_value(*return_value);
                                let instruction_result_type =
                                    dfg.type_of_value(*instruction_result);
                                if return_type != instruction_result_type {
                                    panic!(
                                        "Function call to {} expected return type {}, but got {} (at position {})",
                                        func_id,
                                        instruction_result_type,
                                        return_type,
                                        index + 1
                                    );
                                }
                            }
                        }
                    }
                    _ => (),
                }
            }
            Instruction::Constrain(lhs, rhs, _) | Instruction::ConstrainNotEqual(lhs, rhs, _) => {
                let lhs_type = dfg.type_of_value(*lhs);
                let rhs_type = dfg.type_of_value(*rhs);
                if lhs_type != rhs_type {
                    panic!(
                        "Left-hand side and right-hand side of constrain must have the same type"
                    );
                }
            }
            Instruction::MakeArray { elements, typ: _ } => {
                let results = dfg.instruction_results(instruction);
                assert_eq!(results.len(), 1, "MakeArray must return exactly one value");

                let result_type = dfg.type_of_value(results[0]);

                let composite_type = match result_type {
                    Type::Array(composite_type, length) => {
                        let array_flattened_length = composite_type.len() * length as usize;
                        if elements.len() != array_flattened_length {
                            panic!(
                                "MakeArray returns an array of flattened length {}, but it has {} elements",
                                array_flattened_length,
                                elements.len()
                            );
                        }
                        composite_type
                    }
                    Type::Slice(composite_type) => {
                        if elements.len() % composite_type.len() != 0 {
                            panic!(
                                "MakeArray slice has {} elements but composite type has {} types which don't divide the number of elements",
                                elements.len(),
                                composite_type.len()
                            );
                        }
                        composite_type
                    }
                    _ => {
                        panic!("MakeArray must return an array or slice type, not {result_type}");
                    }
                };

                let composite_type_len = composite_type.len();
                for (index, element) in elements.iter().enumerate() {
                    let element_type = dfg.type_of_value(*element);
                    let expected_type = &composite_type[index % composite_type_len];
                    if &element_type != expected_type {
                        panic!(
                            "MakeArray has incorrect element type at index {index}: expected {}, got {}",
                            expected_type, element_type
                        );
                    }
                }
            }
            _ => (),
        }
    }

    fn type_check_globals(&self) {
        let globals = (*self.function.dfg.globals).clone();
        for (_, global) in globals.values_iter() {
            let global_typ = global.get_type();
            if global_typ.contains_function() {
                panic!("Globals cannot contain function pointers");
            }
        }
    }

    fn run(&mut self) {
        self.type_check_globals();
        self.validate_single_return_block();

        for block in self.function.reachable_blocks() {
            for instruction in self.function.dfg[block].instructions() {
                self.validate_field_to_integer_cast_invariant(*instruction);
                self.type_check_instruction(*instruction);
            }
        }
    }
}

/// Validates that the [Function] is well formed.
///
/// Panics on malformed functions.
pub(crate) fn validate_function(function: &Function, ssa: &Ssa) {
    let mut validator = Validator::new(function, ssa);
    validator.run();
}

#[cfg(test)]
mod tests {
    use crate::ssa::ssa_gen::Ssa;

    #[test]
    fn lone_truncate() {
        let src = r"
        acir(inline) pure fn main f0 {
          b0(v0: i16):
            v1 = truncate v0 to 8 bits, max_bit_size: 8
            return v1
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(expected = "Cannot use `lt` with field elements")]
    fn disallows_comparing_fields_with_lt() {
        let src = "
        acir(inline) impure fn main f0 {
          b0():
            v2 = lt Field 1, Field 2
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(
        expected = "Left-hand side and right-hand side of `add` must have the same type"
    )]
    fn disallows_binary_add_with_different_types() {
        let src = "
        acir(inline) fn main f0 {
          b0():
            v2 = add Field 1, i32 2
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(expected = "Right-hand side of `shr` must be u8")]
    fn disallows_shr_with_non_u8() {
        let src = "
        acir(inline) fn main f0 {
          b0():
            v2 = shr u32 1, u16 1
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(expected = "Right-hand side of `shl` must be u8")]
    fn disallows_shl_with_non_u8() {
        let src = "
        acir(inline) fn main f0 {
          b0():
            v2 = shl u32 1, u16 1
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(
        expected = "assertion failed: matches!(value_typ, Type::Numeric(NumericType::NativeField))"
    )]
    fn to_le_radix_on_non_field_value() {
        let src = "
        brillig(inline) predicate_pure fn main f0 {
          b0():
            call f1(u1 1)
            return
        }
        brillig(inline) fn foo f1 {
          b0(v0: u1):
            v2 = call to_le_radix(v0, u32 256) -> [u7; 1]
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(
        expected = "assertion failed: matches!(value_typ, Type::Numeric(NumericType::NativeField))"
    )]
    fn to_le_bits_on_non_field_value() {
        let src = "
        brillig(inline) predicate_pure fn main f0 {
          b0():
            call f1(u1 1)
            return
        }
        brillig(inline) fn foo f1 {
          b0(v0: u1):
            v2 = call to_le_bits(v0) -> [u1; 32]
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    fn valid_to_le_radix() {
        let src = "
        brillig(inline) predicate_pure fn main f0 {
          b0(v0: Field):
            v1 = call to_le_radix(v0, u32 256) -> [u8; 1]
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    fn valid_to_le_bits() {
        let src = "
        brillig(inline) predicate_pure fn main f0 {
          b0(v0: Field):
            v1 = call to_le_bits(v0) -> [u1; 32]
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[should_panic(
        expected = "Left-hand side and right-hand side of constrain must have the same type"
    )]
    #[test]
    fn constrain_with_different_types() {
        let src = "
        brillig(inline) predicate_pure fn main f0 {
          b0(v0: u8, v1: i8):
            constrain v0 == v1
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[should_panic(
        expected = "Left-hand side and right-hand side of constrain must have the same type"
    )]
    #[test]
    fn constrain_neq_with_different_types() {
        let src = "
        brillig(inline) predicate_pure fn main f0 {
          b0(v0: u8, v1: i8):
            constrain v0 != v1
            return
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    fn cast_from_field_constant_in_range() {
        let src = "
        acir(inline) predicate_pure fn main f0 {
          b0():
            v0 = cast Field 42 as u8
            return v0
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    fn cast_from_field_constant_out_of_range_with_truncate() {
        let src = "
        acir(inline) predicate_pure fn main f0 {
          b0():
            v0 = truncate Field 123456 to 8 bits, max_bit_size: 16
            v1 = cast v0 as u8
            return v1
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    fn cast_from_field_division_safe() {
        let src = "
        acir(inline) predicate_pure fn main f0 {
          b0():
            v0 = div u16 256, u16 256
            v1 = cast v0 as u8
            return v1
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(expected = "Constant too large")]
    fn cast_from_field_constant_too_large() {
        let src = "
        acir(inline) predicate_pure fn main f0 {
          b0():
            v0 = cast Field 300 as u8
            return v0
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(expected = "Invalid cast from Field")]
    fn cast_from_raw_field() {
        let src = "
        acir(inline) predicate_pure fn main f0 {
          b0():
            v0 = add Field 255, Field 1
            v1 = cast v0 as u8
            return v1
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(expected = "assertion")]
    fn cast_after_unsafe_truncate() {
        let src = "
        acir(inline) predicate_pure fn main f0 {
          b0():
            v0 = truncate Field 1000 to 16 bits, max_bit_size: 16
            v1 = cast v0 as u8
            return v1
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(expected = "Globals cannot contain function pointers")]
    fn function_pointer_in_global_array() {
        let src = "
        g2 = make_array [f1, f2] : [function; 2]

        acir(inline) fn main f0 {
          b0(v3: u32, v4: Field):
            v6 = call f1() -> Field
            v8 = call f2() -> Field
            v10 = lt v3, u32 2
            constrain v10 == u1 1
            v12 = array_get g2, index v3 -> function
            v13 = call v12() -> Field
            v14 = eq v13, v4
            constrain v13 == v4
            return
        }
        acir(inline) fn f1 f1 {
          b0():
            return Field 1
        }
        acir(inline) fn f2 f2 {
          b0():
            return Field 2
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(expected = "Function call to f1 expected 2 return values, but got 1")]
    fn call_has_wrong_return_count() {
        let src = "
        acir(inline) fn main f0 {
          b0():
            v0, v1 = call f1() -> (Field, Field)
            return v0
        }

        acir(inline) fn foo f1 {
          b0():
            return Field 1
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(
        expected = "Function call to f1 expected return type u8, but got Field (at position 1)"
    )]
    fn call_has_wrong_return_type() {
        let src = "
        acir(inline) fn main f0 {
          b0():
            v0 = call f1() -> u8
            return v0
        }

        acir(inline) fn foo f1 {
          b0():
            return Field 1
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(
        expected = "MakeArray returns an array of flattened length 2, but it has 3 elements"
    )]
    fn make_array_returns_incorrect_length() {
        let src = "
        acir(inline) fn main f0 {
          b0():
            v0 = make_array [u8 1, u8 2, u8 3] : [u8; 2]
            return v0
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(
        expected = "MakeArray returns an array of flattened length 4, but it has 3 elements"
    )]
    fn make_array_returns_incorrect_length_composite_type() {
        let src = "
        acir(inline) fn main f0 {
          b0():
            v0 = make_array [u8 1, u8 2, u8 3] : [(u8, u8); 2]
            return v0
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(
        expected = "MakeArray slice has 3 elements but composite type has 2 types which don't divide the number of elements"
    )]
    fn make_array_slice_returns_incorrect_length() {
        let src = "
        acir(inline) fn main f0 {
          b0():
            v0 = make_array [u8 1, u8 2, u8 3] : [(u8, u8)]
            return v0
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }

    #[test]
    #[should_panic(
        expected = "MakeArray has incorrect element type at index 1: expected u8, got Field"
    )]
    fn make_array_has_incorrect_element_type() {
        let src = "
        acir(inline) fn main f0 {
          b0():
            v0 = make_array [u8 1, Field 2, u8 3, u8 4] : [(u8, u8); 2]
            return v0
        }
        ";
        let _ = Ssa::from_str(src).unwrap();
    }
}
