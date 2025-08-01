#![forbid(unsafe_code)]
#![cfg_attr(not(test), warn(unused_crate_dependencies, unused_extern_crates))]

pub mod builder;
pub mod runner;
pub mod typed_value;

#[cfg(test)]
mod tests {
    use crate::builder::{FuzzerBuilder, FuzzerBuilderError, InstructionWithTwoArgs};
    use crate::runner::{CompareResults, run_and_compare};
    use crate::typed_value::{TypedValue, ValueType};
    use acvm::acir::native_types::{Witness, WitnessMap};
    use acvm::{AcirField, FieldElement};
    use noirc_driver::{CompileOptions, CompiledProgram};
    use rand::Rng;

    use noirc_evaluator::ssa::ir::instruction::BinaryOp;
    use noirc_evaluator::ssa::ir::types::NumericType;

    struct TestHelper {
        acir_builder: FuzzerBuilder,
        brillig_builder: FuzzerBuilder,
    }

    impl TestHelper {
        fn new() -> Self {
            let acir_builder = FuzzerBuilder::new_acir();
            let brillig_builder = FuzzerBuilder::new_brillig();

            Self { acir_builder, brillig_builder }
        }

        fn insert_variable(&mut self, variable_type: ValueType) -> TypedValue {
            let acir_param = self.acir_builder.insert_variable(variable_type.to_ssa_type());
            let brillig_param = self.brillig_builder.insert_variable(variable_type.to_ssa_type());
            assert_eq!(acir_param, brillig_param);
            acir_param
        }

        fn insert_instruction_double_arg(
            &mut self,
            instruction: InstructionWithTwoArgs,
            first_arg: TypedValue,
            second_arg: TypedValue,
        ) -> (TypedValue, TypedValue) {
            let acir_return =
                instruction(&mut self.acir_builder, first_arg.clone(), second_arg.clone());
            let brillig_return = instruction(&mut self.brillig_builder, first_arg, second_arg);
            (acir_return, brillig_return)
        }

        fn finalize_function(&mut self, return_value: TypedValue) {
            self.acir_builder.finalize_function(&return_value);
            self.brillig_builder.finalize_function(&return_value);
        }
    }

    fn get_witness_map(values: &[FieldElement]) -> WitnessMap<FieldElement> {
        let mut witness_map = WitnessMap::new();
        for (i, value) in values.iter().enumerate() {
            let witness = Witness(i as u32);
            let value = *value;
            witness_map.insert(witness, value);
        }
        witness_map
    }

    fn compare_results<T: Into<FieldElement>>(computed_rust: T, computed_noir: FieldElement) {
        assert_eq!(computed_rust.into(), computed_noir, "Noir doesn't match Rust");
    }

    /// Runs the given instruction with the given values and returns the results of the ACIR and Brillig programs
    /// Instruction runs with first and second witness given
    fn run_instruction_double_arg(
        instruction: InstructionWithTwoArgs,
        lhs: (FieldElement, ValueType),
        rhs: (FieldElement, ValueType),
    ) -> FieldElement {
        let mut test_helper = TestHelper::new();
        let lhs_val = test_helper.insert_variable(lhs.1);
        let rhs_val = test_helper.insert_variable(rhs.1);
        let lhs = lhs.0;
        let rhs = rhs.0;

        let (acir_result, brillig_result) =
            test_helper.insert_instruction_double_arg(instruction, lhs_val, rhs_val);
        test_helper.finalize_function(acir_result);
        test_helper.finalize_function(brillig_result);

        let compile_options = CompileOptions::default();
        let acir_program = test_helper.acir_builder.compile(compile_options.clone()).unwrap();
        let brillig_program = test_helper.brillig_builder.compile(compile_options).unwrap();

        let witness_map = get_witness_map(&[lhs, rhs]);
        let initial_witness = witness_map;
        let compare_results =
            run_and_compare(&acir_program.program, &brillig_program.program, initial_witness);
        // If not agree throw panic, it is not intended to happen in tests
        match compare_results {
            CompareResults::Agree(result) => result,
            CompareResults::Disagree(acir_result, brillig_result) => {
                panic!(
                    "ACIR and Brillig results disagree: ACIR: {acir_result}, Brillig: {brillig_result}, lhs: {lhs}, rhs: {rhs}"
                );
            }
            CompareResults::BothFailed(acir_error, brillig_error) => {
                panic!(
                    "Both ACIR and Brillig failed: ACIR: {acir_error}, Brillig: {brillig_error}, lhs: {lhs}, rhs: {rhs}"
                );
            }
            CompareResults::AcirFailed(acir_error, brillig_result) => {
                panic!(
                    "ACIR failed: ACIR: {acir_error}, Brillig: {brillig_result}, lhs: {lhs}, rhs: {rhs}"
                );
            }
            CompareResults::BrilligFailed(brillig_error, acir_result) => {
                panic!(
                    "Brillig failed: Brillig: {brillig_error}, ACIR: {acir_result}, lhs: {lhs}, rhs: {rhs}"
                );
            }
        }
    }

    #[test]
    fn test_unsigned_add() {
        let mut rng = rand::thread_rng();
        let mut lhs: u64 = rng.r#gen();
        let rhs: u64 = rng.r#gen();

        // to prevent `attempt to add with overflow`
        lhs %= 12341234;
        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_add_instruction_checked,
            (lhs.into(), ValueType::U64),
            (rhs.into(), ValueType::U64),
        );
        compare_results(lhs + rhs, noir_res);
    }

    fn parse_integer_to_signed<T: Into<i128>>(integer: T) -> FieldElement {
        let integer: i128 = integer.into();
        let width = 8 * size_of::<T>();

        let min = if width == 128 { i128::MIN } else { -(1 << (width - 1)) };
        let max = if width == 128 { i128::MAX } else { (1 << (width - 1)) - 1 };

        if integer < min {
            panic!("value is less than min");
        } else if integer > max {
            panic!("value is greater than max");
        }

        if integer < 0 {
            FieldElement::from(2u32).pow(&width.into()) + FieldElement::from(integer)
        } else {
            FieldElement::from(integer)
        }
    }

    #[test]
    fn test_signed_add() {
        let mut rng = rand::thread_rng();
        let mut lhs: i64 = rng.r#gen();
        let rhs: i64 = rng.r#gen();

        // to prevent `attempt to add with overflow`
        lhs %= 12341234;
        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_add_instruction_checked,
            (parse_integer_to_signed(lhs), ValueType::I64),
            (parse_integer_to_signed(rhs), ValueType::I64),
        );
        compare_results(parse_integer_to_signed(lhs + rhs), noir_res);
    }

    #[test]
    fn test_unsigned_sub() {
        let mut rng = rand::thread_rng();
        let mut lhs: u64 = rng.r#gen();
        let mut rhs: u64 = rng.r#gen();

        // to prevent `attempt to subtract with overflow`
        if lhs < rhs {
            (lhs, rhs) = (rhs, lhs);
        }
        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_sub_instruction_checked,
            (lhs.into(), ValueType::U64),
            (rhs.into(), ValueType::U64),
        );
        compare_results(lhs - rhs, noir_res);
    }

    #[test]
    fn test_signed_sub() {
        let mut rng = rand::thread_rng();
        let mut lhs: i64 = rng.r#gen();
        let mut rhs: i64 = rng.r#gen();

        // to prevent `attempt to subtract with overflow`
        lhs %= 12341234;
        rhs %= 12341234;
        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_sub_instruction_checked,
            (parse_integer_to_signed(lhs), ValueType::I64),
            (parse_integer_to_signed(rhs), ValueType::I64),
        );
        compare_results(parse_integer_to_signed(lhs - rhs), noir_res);
    }

    #[test]
    fn test_unsigned_mul() {
        let mut rng = rand::thread_rng();
        let mut lhs: u64 = rng.r#gen();
        let mut rhs: u64 = rng.r#gen();

        // to prevent `attempt to multiply with overflow`
        lhs %= 12341234;
        rhs %= 12341234;
        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_mul_instruction_checked,
            (lhs.into(), ValueType::U64),
            (rhs.into(), ValueType::U64),
        );
        compare_results(lhs * rhs, noir_res);
    }

    #[test]
    fn test_signed_mul() {
        let mut rng = rand::thread_rng();
        let mut lhs: u64 = rng.r#gen();
        let mut rhs: u64 = rng.r#gen();

        // to prevent `attempt to multiply with overflow`
        lhs %= 12341234;
        rhs %= 12341234;
        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_mul_instruction_checked,
            (parse_integer_to_signed(lhs), ValueType::I64),
            (parse_integer_to_signed(rhs), ValueType::I64),
        );
        compare_results(parse_integer_to_signed(lhs * rhs), noir_res);
    }

    #[test]
    fn test_div() {
        let mut rng = rand::thread_rng();
        let lhs: u64 = rng.r#gen();
        let mut rhs: u64 = rng.r#gen();

        if rhs == 0 {
            rhs = 1;
        }
        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_div_instruction,
            (lhs.into(), ValueType::U64),
            (rhs.into(), ValueType::U64),
        );
        compare_results(lhs / rhs, noir_res);
    }

    #[test]
    fn test_mod() {
        let mut rng = rand::thread_rng();
        let lhs: u64 = rng.r#gen();
        let rhs: u64 = rng.r#gen();

        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_mod_instruction,
            (lhs.into(), ValueType::U64),
            (rhs.into(), ValueType::U64),
        );
        compare_results(lhs % rhs, noir_res);
    }

    #[test]
    fn test_and() {
        let mut rng = rand::thread_rng();
        let lhs: u64 = rng.r#gen();
        let rhs: u64 = rng.r#gen();

        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_and_instruction,
            (lhs.into(), ValueType::U64),
            (rhs.into(), ValueType::U64),
        );
        compare_results(lhs & rhs, noir_res);
    }

    #[test]
    fn test_or() {
        let mut rng = rand::thread_rng();
        let lhs: u64 = rng.r#gen();
        let rhs: u64 = rng.r#gen();

        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_or_instruction,
            (lhs.into(), ValueType::U64),
            (rhs.into(), ValueType::U64),
        );
        compare_results(lhs | rhs, noir_res);
    }

    #[test]
    fn test_xor() {
        let mut rng = rand::thread_rng();
        let lhs: u64 = rng.r#gen();
        let rhs: u64 = rng.r#gen();

        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_xor_instruction,
            (lhs.into(), ValueType::U64),
            (rhs.into(), ValueType::U64),
        );
        compare_results(lhs ^ rhs, noir_res);
    }
    #[test]
    fn test_shr() {
        let mut rng = rand::thread_rng();
        let lhs: u64 = rng.r#gen();
        let mut rhs: u64 = rng.r#gen();

        rhs %= 64;
        let noir_res = run_instruction_double_arg(
            FuzzerBuilder::insert_shr_instruction,
            (lhs.into(), ValueType::U64),
            (rhs.into(), ValueType::U64),
        );
        compare_results(lhs >> rhs, noir_res);
    }

    fn check_expected_validation_error(
        compilation_result: Result<CompiledProgram, FuzzerBuilderError>,
        expected_message: &str,
    ) {
        match compilation_result {
            Ok(_) => panic!("Expected an SSA validation failure"),
            Err(FuzzerBuilderError::RuntimeError(error)) => {
                assert!(error.contains(expected_message));
            }
        }
    }

    #[test]
    fn regression_cast_without_truncate() {
        let mut acir_builder = FuzzerBuilder::new_acir();
        let mut brillig_builder = FuzzerBuilder::new_brillig();

        let field_var_acir_id_1 =
            acir_builder.insert_variable(ValueType::Field.to_ssa_type()).value_id;
        let u64_var_acir_id_2 = acir_builder.insert_variable(ValueType::U64.to_ssa_type()).value_id;
        let field_var_brillig_id_1 =
            brillig_builder.insert_variable(ValueType::Field.to_ssa_type()).value_id;
        let u64_var_brillig_id_2 =
            brillig_builder.insert_variable(ValueType::U64.to_ssa_type()).value_id;

        let casted_acir = acir_builder
            .builder
            .insert_cast(field_var_acir_id_1, NumericType::Unsigned { bit_size: 64 });
        let casted_brillig = brillig_builder
            .builder
            .insert_cast(field_var_brillig_id_1, NumericType::Unsigned { bit_size: 64 });

        let mul_acir = acir_builder.builder.insert_binary(
            casted_acir,
            BinaryOp::Mul { unchecked: false },
            u64_var_acir_id_2,
        );
        let mul_brillig = brillig_builder.builder.insert_binary(
            casted_brillig,
            BinaryOp::Mul { unchecked: false },
            u64_var_brillig_id_2,
        );

        acir_builder.builder.terminate_with_return(vec![mul_acir]);
        brillig_builder.builder.terminate_with_return(vec![mul_brillig]);

        let acir_result = acir_builder.compile(CompileOptions::default());
        check_expected_validation_error(
            acir_result,
            "Invalid cast from Field, not preceded by valid truncation or known safe value",
        );
        let brillig_result = brillig_builder.compile(CompileOptions::default());
        check_expected_validation_error(
            brillig_result,
            "Invalid cast from Field, not preceded by valid truncation or known safe value",
        );
    }
}
