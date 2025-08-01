//! This file contains configurations for selecting particular behaviors during mutations
use rand::{Rng, rngs::StdRng};

pub(crate) const MAX_NUMBER_OF_MUTATIONS: usize = 25;
pub(crate) const SIZE_OF_SMALL_ARBITRARY_BUFFER: usize = 25;
pub(crate) const SIZE_OF_LARGE_ARBITRARY_BUFFER: usize = 1024;

pub(crate) struct WeightedSelectionConfig<T, const N: usize> {
    pub(crate) options_with_weights: [(T, usize); N],
    pub(crate) total_weight: usize,
}

impl<T: Copy, const N: usize> WeightedSelectionConfig<T, N> {
    pub(crate) const fn new(options_with_weights: [(T, usize); N]) -> Self {
        let mut total_weight = 0;
        let mut i = 0;
        while i < options_with_weights.len() {
            total_weight += options_with_weights[i].1;
            i += 1;
        }

        Self { options_with_weights, total_weight }
    }

    pub(crate) fn select(&self, rng: &mut StdRng) -> T {
        let mut selector = rng.gen_range(0..self.total_weight);
        for (option, weight) in &self.options_with_weights {
            if selector < *weight {
                return *option;
            }
            selector -= weight;
        }
        unreachable!("Should have returned by now")
    }
}

/// Mutations config for single FuzzerData mutations
#[derive(Copy, Clone, Debug)]
pub(crate) enum FuzzerDataMutationOptions {
    Functions,
    InstructionBlocks,
    Witnesses,
}
pub(crate) type FuzzerDataMutationConfig = WeightedSelectionConfig<FuzzerDataMutationOptions, 3>;
pub(crate) const BASIC_FUZZER_DATA_MUTATION_CONFIGURATION: FuzzerDataMutationConfig =
    FuzzerDataMutationConfig::new([
        (FuzzerDataMutationOptions::Functions, 1),
        (FuzzerDataMutationOptions::InstructionBlocks, 1),
        (FuzzerDataMutationOptions::Witnesses, 4),
    ]);

/// Mutations config for function mutations
#[derive(Copy, Clone, Debug)]
pub(crate) enum FunctionMutationOptions {
    ReturnBlockIdx,
    FunctionFuzzerCommands,
    ReturnType,
}

pub(crate) type MutationConfig = WeightedSelectionConfig<FunctionMutationOptions, 3>;
pub(crate) const BASIC_FUNCTION_MUTATION_CONFIGURATION: MutationConfig = MutationConfig::new([
    (FunctionMutationOptions::ReturnBlockIdx, 0), // TODO(sn): change when implemented
    (FunctionMutationOptions::FunctionFuzzerCommands, 1),
    (FunctionMutationOptions::ReturnType, 0), // TODO(sn): change when implemented
]);

/// Mutations of witness values
#[derive(Copy, Clone, Debug)]
pub(crate) enum WitnessMutationOptions {
    Random,
    MaxValue,
    MinValue,
    SmallAddSub,
    PowerOfTwoAddSub,
}

pub(crate) type WitnessMutationConfig = WeightedSelectionConfig<WitnessMutationOptions, 5>;
pub(crate) const BASIC_WITNESS_MUTATION_CONFIGURATION: WitnessMutationConfig =
    WitnessMutationConfig::new([
        (WitnessMutationOptions::Random, 1),
        (WitnessMutationOptions::MaxValue, 2),
        (WitnessMutationOptions::MinValue, 2),
        (WitnessMutationOptions::SmallAddSub, 4),
        (WitnessMutationOptions::PowerOfTwoAddSub, 3),
    ]);

/// Mutations of fuzzer commands
#[derive(Copy, Clone, Debug)]
pub(crate) enum FuzzerCommandMutationOptions {
    Random,
    RemoveCommand,
    AddCommand,
    ReplaceCommand,
}

pub(crate) type FuzzerCommandMutationConfig =
    WeightedSelectionConfig<FuzzerCommandMutationOptions, 4>;
pub(crate) const BASIC_FUZZER_COMMAND_MUTATION_CONFIGURATION: FuzzerCommandMutationConfig =
    FuzzerCommandMutationConfig::new([
        (FuzzerCommandMutationOptions::Random, 1),
        (FuzzerCommandMutationOptions::RemoveCommand, 2),
        (FuzzerCommandMutationOptions::AddCommand, 4),
        (FuzzerCommandMutationOptions::ReplaceCommand, 3),
    ]);

/// Mutations of vector of instruction blocks
#[derive(Copy, Clone, Debug)]
pub(crate) enum VectorOfInstructionBlocksMutationOptions {
    Random,
    InstructionBlockDeletion,
    InstructionBlockInsertion,
    InstructionBlockMutation,
    InstructionBlockSwap,
}

pub(crate) type VectorOfInstructionBlocksMutationConfig =
    WeightedSelectionConfig<VectorOfInstructionBlocksMutationOptions, 5>;
pub(crate) const BASIC_VECTOR_OF_INSTRUCTION_BLOCKS_MUTATION_CONFIGURATION:
    VectorOfInstructionBlocksMutationConfig = VectorOfInstructionBlocksMutationConfig::new([
    (VectorOfInstructionBlocksMutationOptions::Random, 1),
    (VectorOfInstructionBlocksMutationOptions::InstructionBlockDeletion, 15),
    (VectorOfInstructionBlocksMutationOptions::InstructionBlockInsertion, 15),
    (VectorOfInstructionBlocksMutationOptions::InstructionBlockMutation, 55),
    (VectorOfInstructionBlocksMutationOptions::InstructionBlockSwap, 15),
]);

/// Mutations of single instruction block
#[derive(Copy, Clone, Debug)]
pub(crate) enum InstructionBlockMutationOptions {
    Random,
    InstructionDeletion,
    InstructionInsertion,
    InstructionMutation,
    InstructionSwap,
}

pub(crate) type InstructionBlockMutationConfig =
    WeightedSelectionConfig<InstructionBlockMutationOptions, 5>;
pub(crate) const BASIC_INSTRUCTION_BLOCK_MUTATION_CONFIGURATION: InstructionBlockMutationConfig =
    InstructionBlockMutationConfig::new([
        (InstructionBlockMutationOptions::Random, 1),
        (InstructionBlockMutationOptions::InstructionDeletion, 15),
        (InstructionBlockMutationOptions::InstructionInsertion, 15),
        (InstructionBlockMutationOptions::InstructionMutation, 55),
        (InstructionBlockMutationOptions::InstructionSwap, 15),
    ]);

/// Mutations of instructions
#[derive(Copy, Clone, Debug)]
pub(crate) enum InstructionMutationOptions {
    Random,
    ArgumentMutation,
}

pub(crate) type InstructionMutationConfig = WeightedSelectionConfig<InstructionMutationOptions, 2>;
pub(crate) const BASIC_INSTRUCTION_MUTATION_CONFIGURATION: InstructionMutationConfig =
    InstructionMutationConfig::new([
        (InstructionMutationOptions::Random, 1),
        (InstructionMutationOptions::ArgumentMutation, 4),
    ]);

/// Instruction argument mutation configuration
#[derive(Copy, Clone, Debug)]
pub(crate) enum InstructionArgumentMutationOptions {
    Left,
    Right,
}

pub(crate) type InstructionArgumentMutationConfig =
    WeightedSelectionConfig<InstructionArgumentMutationOptions, 2>;
pub(crate) const BASIC_INSTRUCTION_ARGUMENT_MUTATION_CONFIGURATION:
    InstructionArgumentMutationConfig = InstructionArgumentMutationConfig::new([
    // Fuzzer uses type of the left variable for binary ops,
    // so mutating the right variables makes less sense
    (InstructionArgumentMutationOptions::Left, 5),
    (InstructionArgumentMutationOptions::Right, 1),
]);

/// Mutations of arguments of instructions
#[derive(Copy, Clone, Debug)]
pub(crate) enum ArgumentMutationOptions {
    Random,
    IncrementIndex,
    DecrementIndex,
    ChangeType,
}

pub(crate) type ArgumentMutationConfig = WeightedSelectionConfig<ArgumentMutationOptions, 4>;
pub(crate) const BASIC_ARGUMENT_MUTATION_CONFIGURATION: ArgumentMutationConfig =
    ArgumentMutationConfig::new([
        (ArgumentMutationOptions::Random, 1),
        (ArgumentMutationOptions::IncrementIndex, 3),
        (ArgumentMutationOptions::DecrementIndex, 3),
        (ArgumentMutationOptions::ChangeType, 2),
    ]);

/// Mutations of value types
#[derive(Copy, Clone, Debug)]
pub(crate) enum ValueTypeMutationOptions {
    Random,
}

pub(crate) type ValueTypeMutationConfig = WeightedSelectionConfig<ValueTypeMutationOptions, 1>;
pub(crate) const BASIC_VALUE_TYPE_MUTATION_CONFIGURATION: ValueTypeMutationConfig =
    ValueTypeMutationConfig::new([(ValueTypeMutationOptions::Random, 1)]);

#[derive(Copy, Clone, Debug)]
pub(crate) enum FunctionVecMutationOptions {
    CopyFunction,
    Insertion,
    Remove,
    InsertEmpty,
    MutateFunction,
}
pub(crate) type FunctionVecMutationConfig = WeightedSelectionConfig<FunctionVecMutationOptions, 5>;

pub(crate) const BASIC_FUNCTION_VEC_MUTATION_CONFIGURATION: FunctionVecMutationConfig =
    FunctionVecMutationConfig::new([
        (FunctionVecMutationOptions::CopyFunction, 5),
        (FunctionVecMutationOptions::Insertion, 6),
        (FunctionVecMutationOptions::Remove, 10),
        (FunctionVecMutationOptions::InsertEmpty, 15),
        (FunctionVecMutationOptions::MutateFunction, 40),
    ]);
