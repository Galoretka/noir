pub mod comptime;
pub mod def_collector;
pub mod def_map;
pub mod resolution;
pub mod scope;
pub mod type_check;

use crate::ast::UnresolvedGenerics;
use crate::debug::DebugInstrumenter;
use crate::elaborator::UnstableFeature;
use crate::graph::{CrateGraph, CrateId};
use crate::hir::def_map::DefMaps;
use crate::hir_def::function::FuncMeta;
use crate::node_interner::{FuncId, NodeInterner, TypeId};
use crate::parser::ParserError;
use crate::usage_tracker::UsageTracker;
use crate::{Generics, Kind, ParsedModule, ResolvedGeneric, TypeVariable};
use def_collector::dc_crate::CompilationError;
use def_map::{CrateDefMap, FuzzingHarness, fully_qualified_module_path};
use fm::{FileId, FileManager};
use iter_extended::vecmap;
use noirc_errors::Location;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use self::def_map::TestFunction;

pub type ParsedFiles = HashMap<fm::FileId, (ParsedModule, Vec<ParserError>)>;

/// Helper object which groups together several useful context objects used
/// during name resolution. Once name resolution is finished, only the
/// def_interner is required for type inference and monomorphization.
pub struct Context<'file_manager, 'parsed_files> {
    pub def_interner: NodeInterner,
    pub crate_graph: CrateGraph,
    pub def_maps: BTreeMap<CrateId, CrateDefMap>,
    pub usage_tracker: UsageTracker,
    // In the WASM context, we take ownership of the file manager,
    // which is why this needs to be a Cow. In all use-cases, the file manager
    // is read-only however, once it has been passed to the Context.
    pub file_manager: Cow<'file_manager, FileManager>,

    pub debug_instrumenter: DebugInstrumenter,

    /// A map of each file that already has been visited from a prior `mod foo;` declaration.
    /// This is used to issue an error if a second `mod foo;` is declared to the same file.
    pub visited_files: BTreeMap<fm::FileId, Location>,

    // A map of all parsed files.
    // Same as the file manager, we take ownership of the parsed files in the WASM context.
    // Parsed files is also read only.
    pub parsed_files: Cow<'parsed_files, ParsedFiles>,

    pub package_build_path: PathBuf,

    /// Writer for comptime prints.
    pub interpreter_output: Option<Rc<RefCell<dyn std::io::Write>>>,

    /// Any unstable features required by the current package or its dependencies.
    pub required_unstable_features: BTreeMap<CrateId, Vec<UnstableFeature>>,
}

#[derive(Debug)]
pub enum FunctionNameMatch {
    Anything,
    Exact(Vec<String>),
    Contains(Vec<String>),
}

impl Context<'_, '_> {
    pub fn new(file_manager: FileManager, parsed_files: ParsedFiles) -> Context<'static, 'static> {
        Context {
            def_interner: NodeInterner::default(),
            def_maps: BTreeMap::new(),
            usage_tracker: UsageTracker::default(),
            visited_files: BTreeMap::new(),
            crate_graph: CrateGraph::default(),
            file_manager: Cow::Owned(file_manager),
            debug_instrumenter: DebugInstrumenter::default(),
            parsed_files: Cow::Owned(parsed_files),
            package_build_path: PathBuf::default(),
            interpreter_output: Some(Rc::new(RefCell::new(std::io::stdout()))),
            required_unstable_features: BTreeMap::new(),
        }
    }

    pub fn from_ref_file_manager<'file_manager, 'parsed_files>(
        file_manager: &'file_manager FileManager,
        parsed_files: &'parsed_files ParsedFiles,
    ) -> Context<'file_manager, 'parsed_files> {
        Context {
            def_interner: NodeInterner::default(),
            def_maps: BTreeMap::new(),
            usage_tracker: UsageTracker::default(),
            visited_files: BTreeMap::new(),
            crate_graph: CrateGraph::default(),
            file_manager: Cow::Borrowed(file_manager),
            debug_instrumenter: DebugInstrumenter::default(),
            parsed_files: Cow::Borrowed(parsed_files),
            package_build_path: PathBuf::default(),
            interpreter_output: Some(Rc::new(RefCell::new(std::io::stdout()))),
            required_unstable_features: BTreeMap::new(),
        }
    }

    pub fn parsed_file_results(&self, file_id: FileId) -> (ParsedModule, Vec<ParserError>) {
        self.parsed_files.get(&file_id).expect("noir file wasn't parsed").clone()
    }

    /// Returns the CrateDefMap for a given CrateId.
    /// It is perfectly valid for the compiler to look
    /// up a CrateDefMap and it is not available.
    /// This is how the compiler knows to compile a Crate.
    pub fn def_map(&self, crate_id: &CrateId) -> Option<&CrateDefMap> {
        self.def_maps.get(crate_id)
    }

    pub fn def_map_mut(&mut self, crate_id: &CrateId) -> Option<&mut CrateDefMap> {
        self.def_maps.get_mut(crate_id)
    }

    /// Return the CrateId for each crate that has been compiled
    /// successfully
    pub fn crates(&self) -> impl Iterator<Item = CrateId> + '_ {
        self.crate_graph.iter_keys()
    }

    pub fn root_crate_id(&self) -> &CrateId {
        self.crate_graph.root_crate_id()
    }

    pub fn stdlib_crate_id(&self) -> &CrateId {
        self.crate_graph.stdlib_crate_id()
    }

    // TODO: Decide if we actually need `function_name` and `fully_qualified_function_name`
    pub fn function_name(&self, id: &FuncId) -> &str {
        self.def_interner.function_name(id)
    }

    pub fn fully_qualified_function_name(&self, crate_id: &CrateId, id: &FuncId) -> String {
        fully_qualified_function_name(*crate_id, *id, &self.def_interner, &self.def_maps)
    }

    /// Returns a fully-qualified path to the given [TypeId] from the given [CrateId]. This function also
    /// account for the crate names of dependencies.
    ///
    /// For example, if you project contains a `main.nr` and `foo.nr` and you provide the `main_crate_id` and the
    /// `bar_struct_id` where the `Bar` struct is inside `foo.nr`, this function would return `foo::Bar` as a [String].
    pub fn fully_qualified_struct_path(&self, crate_id: &CrateId, id: TypeId) -> String {
        fully_qualified_module_path(&self.def_maps, &self.crate_graph, crate_id, id.module_id())
    }

    pub fn function_meta(&self, func_id: &FuncId) -> &FuncMeta {
        self.def_interner.function_meta(func_id)
    }

    /// Returns the FuncId of the 'main' function in a crate.
    /// - Expects check_crate to be called beforehand
    /// - Panics if no main function is found
    pub fn get_main_function(&self, crate_id: &CrateId) -> Option<FuncId> {
        // Find the local crate, one should always be present
        let local_crate = self.def_map(crate_id).unwrap();

        local_crate.main_function()
    }

    /// Returns a list of all functions in the current crate marked with #[test]
    /// whose names contain the given pattern string. An empty pattern string
    /// will return all functions marked with #[test].
    pub fn get_all_test_functions_in_crate_matching(
        &self,
        crate_id: &CrateId,
        pattern: &FunctionNameMatch,
    ) -> Vec<(String, TestFunction)> {
        get_all_test_functions_in_crate_matching(
            *crate_id,
            pattern,
            &self.def_interner,
            &self.def_maps,
        )
    }

    /// Returns a list of all functions in the current crate marked with `#[fuzz]`
    /// whose names contain the given pattern string. An empty pattern string
    /// will return all functions marked with `#[fuzz]`.
    pub fn get_all_fuzzing_harnesses_in_crate_matching(
        &self,
        crate_id: &CrateId,
        pattern: &FunctionNameMatch,
    ) -> Vec<(String, FuzzingHarness)> {
        let interner = &self.def_interner;
        let def_map = self.def_map(crate_id).expect("The local crate should be analyzed already");

        def_map
            .get_all_fuzzing_harnesses(interner)
            .filter_map(|fuzzing_harness| {
                let fully_qualified_name =
                    self.fully_qualified_function_name(crate_id, &fuzzing_harness.id);
                match &pattern {
                    FunctionNameMatch::Anything => Some((fully_qualified_name, fuzzing_harness)),
                    FunctionNameMatch::Exact(patterns) => patterns
                        .iter()
                        .any(|pattern| &fully_qualified_name == pattern)
                        .then_some((fully_qualified_name, fuzzing_harness)),
                    FunctionNameMatch::Contains(patterns) => patterns
                        .iter()
                        .any(|pattern| fully_qualified_name.contains(pattern))
                        .then_some((fully_qualified_name, fuzzing_harness)),
                }
            })
            .collect()
    }

    pub fn get_all_exported_functions_in_crate(&self, crate_id: &CrateId) -> Vec<(String, FuncId)> {
        let interner = &self.def_interner;
        let def_map = self.def_map(crate_id).expect("The local crate should be analyzed already");

        def_map
            .get_all_exported_functions(interner)
            .map(|function_id| {
                let function_name = self.function_name(&function_id).to_owned();
                (function_name, function_id)
            })
            .collect()
    }

    pub fn module(&self, module_id: def_map::ModuleId) -> &def_map::ModuleData {
        module_id.module(&self.def_maps)
    }

    /// Generics need to be resolved before elaboration to distinguish
    /// between normal and numeric generics.
    /// This method is expected to be used during definition collection.
    /// Each result is returned in a list rather than returned as a single result as to allow
    /// definition collection to provide an error for each ill-formed numeric generic.
    pub(crate) fn resolve_generics(
        interner: &NodeInterner,
        generics: &UnresolvedGenerics,
        errors: &mut Vec<CompilationError>,
    ) -> Generics {
        vecmap(generics, |generic| {
            // Map the generic to a fresh type variable
            let id = interner.next_type_variable_id();

            let type_var_kind = generic.kind().unwrap_or_else(|err| {
                errors.push(err.into());
                // When there's an error, unify with any other kinds
                Kind::Any
            });
            let type_var = TypeVariable::unbound(id, type_var_kind);
            let ident = generic.ident();
            let location = ident.location();

            // Check for name collisions of this generic
            let name = Rc::new(ident.to_string());

            ResolvedGeneric { name, type_var, location }
        })
    }

    pub fn crate_files(&self, crate_id: &CrateId) -> HashSet<FileId> {
        self.def_maps.get(crate_id).map(|def_map| def_map.file_ids()).unwrap_or_default()
    }

    /// Activates LSP mode, which will track references for all definitions.
    pub fn activate_lsp_mode(&mut self) {
        self.def_interner.lsp_mode = true;
    }

    pub fn disable_comptime_printing(&mut self) {
        self.interpreter_output = None;
    }

    pub fn set_comptime_printing(&mut self, output: Rc<RefCell<dyn std::io::Write>>) {
        self.interpreter_output = Some(output);
    }
}

pub fn get_all_test_functions_in_crate_matching(
    crate_id: CrateId,
    pattern: &FunctionNameMatch,
    interner: &NodeInterner,
    def_maps: &DefMaps,
) -> Vec<(String, TestFunction)> {
    let def_map = def_maps.get(&crate_id).expect("The local crate should be analyzed already");

    def_map
        .get_all_test_functions(interner)
        .filter_map(|test_function| {
            let fully_qualified_name =
                fully_qualified_function_name(crate_id, test_function.id, interner, def_maps);
            match &pattern {
                FunctionNameMatch::Anything => Some((fully_qualified_name, test_function)),
                FunctionNameMatch::Exact(patterns) => patterns
                    .iter()
                    .any(|pattern| &fully_qualified_name == pattern)
                    .then_some((fully_qualified_name, test_function)),
                FunctionNameMatch::Contains(patterns) => patterns
                    .iter()
                    .any(|pattern| fully_qualified_name.contains(pattern))
                    .then_some((fully_qualified_name, test_function)),
            }
        })
        .collect()
}

fn fully_qualified_function_name(
    crate_id: CrateId,
    id: FuncId,
    interner: &NodeInterner,
    def_maps: &DefMaps,
) -> String {
    let def_map = def_maps.get(&crate_id).expect("The local crate should be analyzed already");

    let name = interner.function_name(&id);

    let module_id = interner.function_module(id);
    let module = module_id.module(def_maps);

    let parent = def_map.get_module_path_with_separator(module_id.local_id, module.parent, "::");

    if parent.is_empty() { name.into() } else { format!("{parent}::{name}") }
}
