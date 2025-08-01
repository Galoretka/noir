use acvm::{FieldElement, acir::AcirField};

use crate::ssa::ir::{
    dfg::DataFlowGraph,
    instruction::{Binary, BinaryOp, ConstrainError, Instruction},
    types::{NumericType, Type},
    value::{Value, ValueId},
};

/// Try to decompose this constrain instruction. This constraint will be broken down such that it instead constrains
/// all the values which are used to compute the values which were being constrained.
pub(super) fn decompose_constrain(
    lhs: ValueId,
    rhs: ValueId,
    msg: &Option<ConstrainError>,
    dfg: &mut DataFlowGraph,
) -> Vec<Instruction> {
    if lhs == rhs {
        // Remove trivial case `assert_eq(x, x)`
        Vec::new()
    } else {
        match (&dfg[lhs], &dfg[rhs]) {
            (Value::NumericConstant { constant, typ }, Value::Instruction { instruction, .. })
            | (Value::Instruction { instruction, .. }, Value::NumericConstant { constant, typ })
                if *typ == NumericType::bool() =>
            {
                match dfg[*instruction] {
                    Instruction::Binary(Binary { lhs, rhs, operator: BinaryOp::Eq })
                        if constant.is_one() =>
                    {
                        // Replace an explicit two step equality assertion
                        //
                        // v2 = eq v0, u32 v1
                        // constrain v2 == u1 1
                        //
                        // with a direct assertion of equality between the two values
                        //
                        // v2 = eq v0, u32 v1
                        // constrain v0 == v1
                        //
                        // Note that this doesn't remove the value `v2` as it may be used in other instructions, but it
                        // will likely be removed through dead instruction elimination.

                        vec![Instruction::Constrain(lhs, rhs, msg.clone())]
                    }

                    Instruction::Binary(Binary { lhs, rhs, operator: BinaryOp::Mul { .. } })
                        if constant.is_one() && dfg.type_of_value(lhs) == Type::bool() =>
                    {
                        // Replace an equality assertion on a boolean multiplication
                        //
                        // v2 = mul v0, v1
                        // constrain v2 == u1 1
                        //
                        // with a direct assertion that each value is equal to 1
                        //
                        // v2 = mul v0, v1
                        // constrain v0 == 1
                        // constrain v1 == 1
                        //
                        // This is due to the fact that for `v2` to be 1 then both `v0` and `v1` are 1.
                        //
                        // Note that this doesn't remove the value `v2` as it may be used in other instructions, but it
                        // will likely be removed through dead instruction elimination.
                        let one = FieldElement::one();
                        let one = dfg.make_constant(one, NumericType::bool());

                        [
                            decompose_constrain(lhs, one, msg, dfg),
                            decompose_constrain(rhs, one, msg, dfg),
                        ]
                        .concat()
                    }

                    Instruction::Binary(Binary { lhs, rhs, operator: BinaryOp::Or })
                        if constant.is_zero() =>
                    {
                        // Replace an equality assertion on an OR
                        //
                        // v2 = or v0, v1
                        // constrain v2 == u1 0
                        //
                        // with a direct assertion that each value is equal to 0
                        //
                        // v2 = or v0, v1
                        // constrain v0 == 0
                        // constrain v1 == 0
                        //
                        // This is due to the fact that for `v2` to be 0 then both `v0` and `v1` are 0.
                        //
                        // Note that this doesn't remove the value `v2` as it may be used in other instructions, but it
                        // will likely be removed through dead instruction elimination.
                        let zero = FieldElement::zero();
                        let zero = dfg.make_constant(zero, dfg.type_of_value(lhs).unwrap_numeric());

                        [
                            decompose_constrain(lhs, zero, msg, dfg),
                            decompose_constrain(rhs, zero, msg, dfg),
                        ]
                        .concat()
                    }

                    Instruction::Not(value) => {
                        // Replace an assertion that a not instruction is truthy
                        //
                        // v1 = not v0
                        // constrain v1 == u1 1
                        //
                        // with an assertion that the not instruction input is falsy
                        //
                        // v1 = not v0
                        // constrain v0 == u1 0
                        //
                        // Note that this doesn't remove the value `v1` as it may be used in other instructions, but it
                        // will likely be removed through dead instruction elimination.
                        let reversed_constant = FieldElement::from(!constant.is_one());
                        let reversed_constant =
                            dfg.make_constant(reversed_constant, NumericType::bool());
                        decompose_constrain(value, reversed_constant, msg, dfg)
                    }

                    _ => vec![Instruction::Constrain(lhs, rhs, msg.clone())],
                }
            }

            (Value::NumericConstant { constant, .. }, Value::Instruction { instruction, .. })
            | (Value::Instruction { instruction, .. }, Value::NumericConstant { constant, .. }) => {
                match dfg[*instruction] {
                    Instruction::Binary(Binary { lhs, rhs, operator: BinaryOp::Mul { .. } })
                        if constant.is_zero() && lhs == rhs =>
                    {
                        // Replace an assertion that a squared value is zero
                        //
                        // v1 = mul v0, v0
                        // constrain v1 == u1 0
                        //
                        // with a direct assertion that value being squared is equal to 0
                        //
                        // v1 = mul v0, v0
                        // constrain v0 == u1 0
                        //
                        // This is due to the fact that for `v1` to be 0 then `v0` is 0.
                        //
                        // Note that this doesn't remove the value `v1` as it may be used in other instructions, but it
                        // will likely be removed through dead instruction elimination.
                        //
                        // This is safe for all numeric types as the underlying field has a prime modulus so squaring
                        // a non-zero value should never result in zero.

                        let zero = FieldElement::zero();
                        let zero = dfg.make_constant(zero, dfg.type_of_value(lhs).unwrap_numeric());
                        decompose_constrain(lhs, zero, msg, dfg)
                    }

                    // Casting a value just to constrain it to a constant.
                    //
                    // Where the constant is representable in the original type we can perform the constraint
                    // on the pre-cast value.
                    Instruction::Cast(val, _) => {
                        let original_typ = dfg.type_of_value(val).unwrap_numeric();
                        let original_typ_max_value =
                            original_typ.max_value().map(|max_value| *constant < max_value);

                        match original_typ_max_value {
                            Ok(true) => {
                                // Constant fits in original type so we can assert against equivalent constant on pre-cast
                                // value.
                                let downcasted_constant =
                                    dfg.make_constant(*constant, original_typ);
                                vec![Instruction::Constrain(val, downcasted_constant, msg.clone())]
                            }
                            Ok(false) | Err(_) => {
                                // Constant does not fit in original type or type does not have `max_value` defined.
                                // We then leave in the cast.
                                vec![Instruction::Constrain(lhs, rhs, msg.clone())]
                            }
                        }
                    }

                    _ => vec![Instruction::Constrain(lhs, rhs, msg.clone())],
                }
            }

            (
                Value::Instruction { instruction: instruction_lhs, .. },
                Value::Instruction { instruction: instruction_rhs, .. },
            ) => {
                match (&dfg[*instruction_lhs], &dfg[*instruction_rhs]) {
                    // Casting two values just to enforce an equality on them.
                    //
                    // This is equivalent to enforcing equality on the original values.
                    (Instruction::Cast(original_lhs, _), Instruction::Cast(original_rhs, _))
                        if dfg.type_of_value(*original_lhs) == dfg.type_of_value(*original_rhs) =>
                    {
                        vec![Instruction::Constrain(*original_lhs, *original_rhs, msg.clone())]
                    }

                    _ => vec![Instruction::Constrain(lhs, rhs, msg.clone())],
                }
            }
            _ => vec![Instruction::Constrain(lhs, rhs, msg.clone())],
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{assert_ssa_snapshot, ssa::ssa_gen::Ssa};

    #[test]
    fn simplifies_assertions_that_squared_values_are_equal_to_zero() {
        let src = "
        acir(inline) fn main f0 {
          b0(v0: Field):
            v1 = mul v0, v0
            constrain v1 == Field 0
            return
        }
        ";
        let ssa = Ssa::from_str_simplifying(src).unwrap();

        assert_ssa_snapshot!(ssa, @r"
        acir(inline) fn main f0 {
          b0(v0: Field):
            v1 = mul v0, v0
            constrain v0 == Field 0
            return
        }
        ");
    }

    #[test]
    fn simplifies_out_noop_bitwise_ands() {
        // Regression test for https://github.com/noir-lang/noir/issues/7451
        let src = "
        acir(inline) predicate_pure fn main f0 {
          b0(v0: u8):
            v1 = and u8 255, v0
            return v1
        }
        ";

        let ssa = Ssa::from_str_simplifying(src).unwrap();

        assert_ssa_snapshot!(ssa, @r"
        acir(inline) predicate_pure fn main f0 {
          b0(v0: u8):
            return v0
        }
        ");
    }

    #[test]
    fn constraint_decomposition() {
        // When constructing this IR, we should automatically decompose the constraint to be in terms of `v0`, `v1` and `v2`.
        //
        // The mul instructions are retained and will be removed in the dead instruction elimination pass.
        let src = "
            acir(inline) fn main f0 {
              b0(v0: u1, v1: u1, v2: u1):
                v3 = mul v0, v1
                v4 = not v2
                v5 = mul v3, v4
                constrain v5 == u1 1
                return
            }
            ";
        let ssa = Ssa::from_str_simplifying(src).unwrap();

        assert_ssa_snapshot!(ssa, @r"
        acir(inline) fn main f0 {
          b0(v0: u1, v1: u1, v2: u1):
            v3 = unchecked_mul v0, v1
            v4 = not v2
            v5 = unchecked_mul v3, v4
            constrain v0 == u1 1
            constrain v1 == u1 1
            constrain v2 == u1 0
            return
        }
        ");
    }
}
