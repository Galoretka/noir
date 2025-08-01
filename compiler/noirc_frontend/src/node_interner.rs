use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;
use std::marker::Copy;

use fm::FileId;
use iter_extended::vecmap;
use noirc_arena::{Arena, Index};
use noirc_errors::{Location, Span};
use petgraph::algo::tarjan_scc;
use petgraph::prelude::DiGraph;
use petgraph::prelude::NodeIndex as PetGraphIndex;
use rustc_hash::FxHashMap as HashMap;

use crate::QuotedType;
use crate::ast::{
    ExpressionKind, Ident, LValue, Pattern, StatementKind, UnaryOp, UnresolvedTypeData,
};
use crate::graph::CrateId;
use crate::hir::comptime;
use crate::hir::def_collector::dc_crate::CompilationError;
use crate::hir::def_collector::dc_crate::{UnresolvedTrait, UnresolvedTypeAlias};
use crate::hir::def_map::DefMaps;
use crate::hir::def_map::{LocalModuleId, ModuleDefId, ModuleId};
use crate::hir::type_check::generics::TraitGenerics;
use crate::hir_def::traits::NamedType;
use crate::hir_def::traits::ResolvedTraitBound;
use crate::locations::AutoImportEntry;
use crate::token::MetaAttribute;
use crate::token::MetaAttributeName;

use crate::GenericTypeVars;
use crate::Generics;
use crate::TraitAssociatedType;
use crate::ast::{BinaryOpKind, FunctionDefinition, ItemVisibility};
use crate::hir::resolution::errors::ResolverError;
use crate::hir_def::expr::HirIdent;
use crate::hir_def::stmt::HirLetStatement;
use crate::hir_def::traits::TraitImpl;
use crate::hir_def::traits::{Trait, TraitConstraint};
use crate::hir_def::types::{DataType, Kind, Type};
use crate::hir_def::{
    expr::HirExpression,
    function::{FuncMeta, HirFunction},
    stmt::HirStatement,
};
use crate::locations::LocationIndices;
use crate::token::{Attributes, SecondaryAttribute};
use crate::{Shared, TypeAlias, TypeBindings, TypeVariable, TypeVariableId};

/// An arbitrary number to limit the recursion depth when searching for trait impls.
/// This is needed to stop recursing for cases such as `impl<T> Foo for T where T: Eq`
const IMPL_SEARCH_RECURSION_LIMIT: u32 = 10;

#[derive(Debug)]
pub struct ModuleAttributes {
    pub name: String,
    pub location: Location,
    pub parent: Option<LocalModuleId>,
    pub visibility: ItemVisibility,
}

type TypeAttributes = Vec<SecondaryAttribute>;

/// The node interner is the central storage location of all nodes in Noir's Hir (the
/// various node types can be found in hir_def). The interner is also used to collect
/// extra information about the Hir, such as the type of each node, information about
/// each definition or struct, etc. Because it is used on the Hir, the NodeInterner is
/// useful in passes where the Hir is used - name resolution, type checking, and
/// monomorphization - and it is not useful afterward.
#[derive(Debug)]
pub struct NodeInterner {
    pub(crate) nodes: Arena<Node>,
    pub(crate) func_meta: HashMap<FuncId, FuncMeta>,

    function_definition_ids: HashMap<FuncId, DefinitionId>,

    // For a given function ID, this gives the function's modifiers which includes
    // its visibility and whether it is unconstrained, among other information.
    // Unlike func_meta, this map is filled out during definition collection rather than name resolution.
    function_modifiers: HashMap<FuncId, FunctionModifiers>,

    // Contains the source module each function was defined in
    function_modules: HashMap<FuncId, ModuleId>,

    // The location of each module
    module_attributes: HashMap<ModuleId, ModuleAttributes>,

    /// This graph tracks dependencies between different global definitions.
    /// This is used to ensure the absence of dependency cycles for globals and types.
    dependency_graph: DiGraph<DependencyId, ()>,

    /// To keep track of where each DependencyId is in `dependency_graph`, we need
    /// this separate graph to map between the ids and indices.
    dependency_graph_indices: HashMap<DependencyId, PetGraphIndex>,

    // Map each `Index` to it's own location
    pub(crate) id_to_location: HashMap<Index, Location>,

    // Maps each DefinitionId to a DefinitionInfo.
    definitions: Vec<DefinitionInfo>,

    // Type checking map
    //
    // This should only be used with indices from the `nodes` arena.
    // Otherwise the indices used may overwrite other existing indices.
    // Each type for each index is filled in during type checking.
    id_to_type: HashMap<Index, Type>,

    // Similar to `id_to_type` but maps definitions to their type
    definition_to_type: HashMap<DefinitionId, Type>,

    // Struct and Enum map.
    //
    // Each type definition is possibly shared across multiple type nodes.
    // It is also mutated through the RefCell during name resolution to append
    // methods from impls to the type.
    data_types: HashMap<TypeId, Shared<DataType>>,

    type_attributes: HashMap<TypeId, TypeAttributes>,

    // Maps TypeAliasId -> Shared<TypeAlias>
    //
    // Map type aliases to the actual type.
    // When resolving types, check against this map to see if a type alias is defined.
    pub(crate) type_aliases: Vec<Shared<TypeAlias>>,

    /// Each trait associated type. These are tracked so that we can distinguish them
    /// from other types and know that, when directly referenced, they should also
    /// lead to an "ambiguous associated type" error.
    pub(crate) trait_associated_types: Vec<TraitAssociatedType>,

    // Trait map.
    //
    // Each trait definition is possibly shared across multiple type nodes.
    // It is also mutated through the RefCell during name resolution to append
    // methods from impls to the type.
    pub(crate) traits: HashMap<TraitId, Trait>,

    // Trait implementation map
    // For each type that implements a given Trait ( corresponding TraitId), there should be an entry here
    // The purpose for this hashmap is to detect duplication of trait implementations ( if any )
    //
    // Indexed by TraitImplIds
    pub(crate) trait_implementations: HashMap<TraitImplId, Shared<TraitImpl>>,

    next_trait_implementation_id: usize,

    /// The associated types for each trait impl.
    /// This is stored outside of the TraitImpl object since it is required before that object is
    /// created, when resolving the type signature of each method in the impl.
    trait_impl_associated_types: HashMap<TraitImplId, Vec<NamedType>>,

    trait_impl_associated_constants: HashMap<TraitImplId, HashMap<String, (DefinitionId, Type)>>,

    /// Trait implementations on each type. This is expected to always have the same length as
    /// `self.trait_implementations`.
    ///
    /// For lack of a better name, this maps a trait id and type combination
    /// to a corresponding impl if one is available for the type. Due to generics,
    /// we cannot map from Type directly to impl, we need to iterate a Vec of all impls
    /// of that trait to see if any type may match. This can be further optimized later
    /// by splitting it up by type.
    trait_implementation_map: HashMap<TraitId, Vec<(Type, TraitImplKind)>>,

    /// When impls are found during type checking, we tag the function call's Ident
    /// with the impl that was selected. For cases with where clauses, this may be
    /// an Assumed (but verified) impl. In this case the monomorphizer should have
    /// the context to get the concrete type of the object and select the correct impl itself.
    selected_trait_implementations: HashMap<ExprId, TraitImplKind>,

    /// Holds the trait ids of the traits used for infix operator overloading
    infix_operator_traits: HashMap<BinaryOpKind, TraitId>,

    /// Holds the trait ids of the traits used for prefix operator overloading
    prefix_operator_traits: HashMap<UnaryOp, TraitId>,

    /// The `Ordering` type is a semi-builtin type that is the result of the comparison traits.
    ordering_type: Option<Type>,

    /// Map from ExprId (referring to a Function/Method call) to its corresponding TypeBindings,
    /// filled out during type checking from instantiated variables. Used during monomorphization
    /// to map call site types back onto function parameter types, and undo this binding as needed.
    pub instantiation_bindings: HashMap<ExprId, TypeBindings>,

    /// Remembers the field index a given HirMemberAccess expression was resolved to during type
    /// checking.
    field_indices: HashMap<ExprId, usize>,

    // Maps GlobalId -> GlobalInfo
    // NOTE: currently only used for checking repeat globals and restricting their scope to a module
    globals: Vec<GlobalInfo>,
    global_attributes: HashMap<GlobalId, Vec<SecondaryAttribute>>,

    next_type_variable_id: std::cell::Cell<usize>,

    /// A map from a type and method name to a function id for the method.
    /// This can resolve to potentially multiple methods if the same method name is
    /// specialized for different generics on the same type. E.g. for `Struct<T>`, we
    /// may have both `impl Struct<u32> { fn foo(){} }` and `impl Struct<u8> { fn foo(){} }`.
    /// If this happens, the returned Vec will have 2 entries and we'll need to further
    /// disambiguate them by checking the type of each function.
    methods: HashMap<TypeMethodKey, HashMap<String, Methods>>,

    // For trait implementation functions, this is their self type and trait they belong to
    func_id_to_trait: HashMap<FuncId, (Type, TraitId)>,

    /// A list of all type aliases that are referenced in the program.
    /// Searched by LSP to resolve [Location]s of [TypeAlias]s
    pub(crate) type_alias_ref: Vec<(TypeAliasId, Location)>,

    /// Stores the [Location] of a [Type] reference
    pub(crate) type_ref_locations: Vec<(Type, Location)>,

    /// In Noir's metaprogramming, a noir type has the type `Type`. When these are spliced
    /// into `quoted` expressions, we preserve the original type by assigning it a unique id
    /// and creating a `Token::QuotedType(id)` from this id. We cannot create a token holding
    /// the actual type since types do not implement Send or Sync.
    quoted_types: noirc_arena::Arena<Type>,

    // Interned `ExpressionKind`s during comptime code.
    interned_expression_kinds: noirc_arena::Arena<ExpressionKind>,

    // Interned `StatementKind`s during comptime code.
    interned_statement_kinds: noirc_arena::Arena<StatementKind>,

    // Interned `UnresolvedTypeData`s during comptime code.
    interned_unresolved_type_data: noirc_arena::Arena<UnresolvedTypeData>,

    // Interned `Pattern`s during comptime code.
    interned_patterns: noirc_arena::Arena<Pattern>,

    /// Determines whether to run in LSP mode. In LSP mode references are tracked.
    pub(crate) lsp_mode: bool,

    /// Store the location of the references in the graph.
    /// Edges are directed from reference nodes to referenced nodes.
    /// For example:
    ///
    /// ```text
    /// let foo = 3;
    /// //  referenced
    /// //   ^
    /// //   |
    /// //   +------------+
    /// let bar = foo;    |
    /// //      reference |
    /// //         v      |
    /// //         |      |
    /// //         +------+
    /// ```
    pub(crate) reference_graph: DiGraph<ReferenceId, ()>,

    /// Tracks the index of the references in the graph
    pub(crate) reference_graph_indices: HashMap<ReferenceId, PetGraphIndex>,

    /// Store the location of the references in the graph
    pub(crate) location_indices: LocationIndices,

    // All names (and their definitions) that can be offered for auto_import.
    // The third value in the tuple is the module where the definition is (only for pub use).
    // These include top-level functions, global variables and types, but excludes
    // impl and trait-impl methods.
    pub(crate) auto_import_names: HashMap<String, Vec<AutoImportEntry>>,

    /// Each value currently in scope in the comptime interpreter.
    /// Each element of the Vec represents a scope with every scope together making
    /// up all currently visible definitions. The first scope is always the global scope.
    ///
    /// This is stored in the NodeInterner so that the Elaborator from each crate can
    /// share the same global values.
    pub(crate) comptime_scopes: Vec<HashMap<DefinitionId, comptime::Value>>,

    /// Captures the documentation comments for each module, struct, trait, function, etc.
    pub(crate) doc_comments: HashMap<ReferenceId, Vec<String>>,

    /// A map of ModuleDefId to each module that pub or pub(crate) exports it.
    /// This is used to offer importing the item via one of these exports if
    /// the item is not visible where it's defined.
    pub reexports: HashMap<ModuleDefId, Vec<Reexport>>,
}

/// A dependency in the dependency graph may be a type or a definition.
/// Types can depend on definitions too. E.g. `Foo` depends on `COUNT` in:
///
/// ```struct
/// global COUNT = 3;
///
/// struct Foo {
///     array: [Field; COUNT],
/// }
/// ```
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum DependencyId {
    Struct(TypeId),
    Global(GlobalId),
    Function(FuncId),
    Alias(TypeAliasId),
    Trait(TraitId),
    Variable(Location),
}

/// A reference to a module, struct, trait, etc., mainly used by the LSP code
/// to keep track of how symbols reference each other.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ReferenceId {
    Module(ModuleId),
    Type(TypeId),
    StructMember(TypeId, usize),
    EnumVariant(TypeId, usize),
    Trait(TraitId),
    TraitAssociatedType(TraitAssociatedTypeId),
    Global(GlobalId),
    Function(FuncId),
    Alias(TypeAliasId),
    Local(DefinitionId),
    Reference(Location, bool /* is Self */),
}

impl ReferenceId {
    pub fn is_self_type_name(&self) -> bool {
        matches!(self, Self::Reference(_, true))
    }
}

/// A trait implementation is either a normal implementation that is present in the source
/// program via an `impl` block, or it is assumed to exist from a `where` clause or similar.
#[derive(Debug, Clone)]
pub enum TraitImplKind {
    Normal(TraitImplId),

    /// Assumed impls don't have an impl id since they don't link back to any concrete part of the source code.
    Assumed {
        object_type: Type,

        /// The trait generics to use - if specified.
        /// This is allowed to be empty when they are inferred. E.g. for:
        ///
        /// ```
        /// trait Into<T> {
        ///     fn into(self) -> T;
        /// }
        /// ```
        ///
        /// The reference `Into::into(x)` would have inferred generics, but
        /// `x.into()` with a `X: Into<Y>` in scope would not.
        trait_generics: TraitGenerics,
    },
}

/// When searching for a trait impl, these are the types of errors we can expect
pub enum ImplSearchErrorKind {
    TypeAnnotationsNeededOnObjectType,
    Nested(Vec<TraitConstraint>),
    MultipleMatching(Vec<String>),
}

/// Represents the methods on a given type that each share the same name.
///
/// Methods are split into inherent methods and trait methods. If there is
/// ever a name that is defined on both a type directly, and defined indirectly
/// via a trait impl, the direct (inherent) name will always take precedence.
///
/// Additionally, types can define specialized impls with methods of the same name
/// as long as these specialized impls do not overlap. E.g. `impl Struct<u32>` and `impl Struct<u64>`
#[derive(Default, Debug, Clone)]
pub struct Methods {
    pub direct: Vec<ImplMethod>,
    pub trait_impl_methods: Vec<TraitImplMethod>,
}

#[derive(Debug, Clone)]
pub struct ImplMethod {
    pub typ: Type,
    pub method: FuncId,
}

#[derive(Debug, Clone)]
pub struct TraitImplMethod {
    pub typ: Type,
    pub method: FuncId,
    pub trait_id: TraitId,
}

/// All the information from a function that is filled out during definition collection rather than
/// name resolution. As a result, if information about a function is needed during name resolution,
/// this is the only place where it is safe to retrieve it (where all fields are guaranteed to be initialized).
#[derive(Debug, Clone)]
pub struct FunctionModifiers {
    pub name: String,

    /// Whether the function is `pub` or not.
    pub visibility: ItemVisibility,

    pub attributes: Attributes,

    pub is_unconstrained: bool,

    pub generic_count: usize,

    pub is_comptime: bool,

    /// The location of the function's name rather than the entire function
    pub name_location: Location,
}

impl FunctionModifiers {
    /// A semi-reasonable set of default FunctionModifiers used for testing.
    #[cfg(test)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            name: String::new(),
            visibility: ItemVisibility::Public,
            attributes: Attributes::empty(),
            is_unconstrained: false,
            generic_count: 0,
            is_comptime: false,
            name_location: Location::dummy(),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct DefinitionId(usize);

impl DefinitionId {
    //dummy id for error reporting
    pub fn dummy_id() -> DefinitionId {
        DefinitionId(usize::MAX)
    }
}

/// An ID for a global value
#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy, PartialOrd, Ord)]
pub struct GlobalId(usize);

impl GlobalId {
    // Dummy id for error reporting
    pub fn dummy_id() -> Self {
        GlobalId(usize::MAX)
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
pub struct StmtId(Index);

impl StmtId {
    //dummy id for error reporting
    // This can be anything, as the program will ultimately fail
    // after resolution
    pub fn dummy_id() -> StmtId {
        StmtId(Index::dummy())
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone, PartialOrd, Ord)]
pub struct ExprId(Index);

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone)]
pub struct FuncId(Index);

impl FuncId {
    //dummy id for error reporting
    // This can be anything, as the program will ultimately fail
    // after resolution
    pub fn dummy_id() -> FuncId {
        FuncId(Index::dummy())
    }
}

impl fmt::Display for FuncId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone, PartialOrd, Ord)]
pub struct TypeId(ModuleId);

impl TypeId {
    //dummy id for error reporting
    // This can be anything, as the program will ultimately fail
    // after resolution
    pub fn dummy_id() -> TypeId {
        TypeId(ModuleId { krate: CrateId::dummy_id(), local_id: LocalModuleId::dummy_id() })
    }

    pub fn module_id(self) -> ModuleId {
        self.0
    }

    pub fn krate(self) -> CrateId {
        self.0.krate
    }

    pub fn local_module_id(self) -> LocalModuleId {
        self.0.local_id
    }

    /// Returns the module where this struct is defined.
    pub fn parent_module_id(self, def_maps: &DefMaps) -> ModuleId {
        self.module_id().parent(def_maps).expect("Expected struct module parent to exist")
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone, PartialOrd, Ord)]
pub struct TypeAliasId(pub usize);

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraitId(pub ModuleId);

impl TraitId {
    // dummy id for error reporting
    // This can be anything, as the program will ultimately fail
    // after resolution
    pub fn dummy_id() -> TraitId {
        TraitId(ModuleId { krate: CrateId::dummy_id(), local_id: LocalModuleId::dummy_id() })
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone, PartialOrd, Ord)]
pub struct TraitAssociatedTypeId(pub usize);

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
pub struct TraitImplId(pub usize);

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct TraitItemId {
    pub trait_id: TraitId,
    /// This is the definition id of the method or associated constant in the trait, not an impl
    pub item_id: DefinitionId,
}

macro_rules! into_index {
    ($id_type:ty) => {
        impl From<$id_type> for Index {
            fn from(t: $id_type) -> Self {
                t.0
            }
        }

        impl From<&$id_type> for Index {
            fn from(t: &$id_type) -> Self {
                t.0
            }
        }
    };
}

into_index!(ExprId);
into_index!(StmtId);

/// A Definition enum specifies anything that we can intern in the NodeInterner
/// We use one Arena for all types that can be interned as that has better cache locality
/// This data structure is never accessed directly, so API wise there is no difference between using
/// Multiple arenas and a single Arena
#[derive(Debug, Clone)]
pub(crate) enum Node {
    Function(HirFunction),
    Statement(HirStatement),
    Expression(HirExpression),
}

#[derive(Debug, Clone)]
pub struct DefinitionInfo {
    pub name: String,
    pub mutable: bool,
    pub comptime: bool,
    pub kind: DefinitionKind,
    pub location: Location,
}

impl DefinitionInfo {
    /// True if this definition is for a global variable.
    /// Note that this returns false for top-level functions.
    pub fn is_global(&self) -> bool {
        self.kind.is_global()
    }

    pub fn is_comptime_local(&self) -> bool {
        self.comptime && matches!(self.kind, DefinitionKind::Local(..))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DefinitionKind {
    Function(FuncId),

    Global(GlobalId),

    /// Locals may be defined in let statements or parameters,
    /// in which case they will not have an associated ExprId
    Local(Option<ExprId>),

    /// Generic types in functions (T, U in `fn foo<T, U>(...)` are declared as variables
    /// in scope in case they resolve to numeric generics later.
    NumericGeneric(TypeVariable, Box<Type>),

    AssociatedConstant(TraitImplId, String),
}

impl DefinitionKind {
    /// True if this definition is for a global variable.
    /// Note that this returns false for top-level functions.
    pub fn is_global(&self) -> bool {
        matches!(self, DefinitionKind::Global(..))
    }

    pub fn get_rhs(&self) -> Option<ExprId> {
        match self {
            DefinitionKind::Function(_) => None,
            DefinitionKind::Global(_) => None,
            DefinitionKind::Local(id) => *id,
            DefinitionKind::NumericGeneric(_, _) => None,
            DefinitionKind::AssociatedConstant(_, _) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GlobalInfo {
    pub id: GlobalId,
    pub definition_id: DefinitionId,
    pub ident: Ident,
    pub visibility: ItemVisibility,
    pub local_id: LocalModuleId,
    pub crate_id: CrateId,
    pub location: Location,
    pub let_statement: StmtId,
    pub value: GlobalValue,
}

#[derive(Debug, Clone)]
pub enum GlobalValue {
    Unresolved,
    Resolving,
    Resolved(comptime::Value),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QuotedTypeId(noirc_arena::Index);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InternedExpressionKind(noirc_arena::Index);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InternedStatementKind(noirc_arena::Index);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InternedUnresolvedTypeData(noirc_arena::Index);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InternedPattern(noirc_arena::Index);

/// Captures a reexport that happens inside a module. For example:
///
/// ```noir
/// mod moo {
/// //  ^^^ module_id
///
///   pub use foo::bar as baz;
/// //^^^ visibility      ^^^ name
/// }
/// ```
///
#[derive(Debug, Clone)]
pub struct Reexport {
    pub module_id: ModuleId,
    pub name: Ident,
    pub visibility: ItemVisibility,
}

impl Default for NodeInterner {
    fn default() -> Self {
        NodeInterner {
            nodes: Arena::default(),
            func_meta: HashMap::default(),
            function_definition_ids: HashMap::default(),
            function_modifiers: HashMap::default(),
            function_modules: HashMap::default(),
            module_attributes: HashMap::default(),
            func_id_to_trait: HashMap::default(),
            dependency_graph: petgraph::graph::DiGraph::new(),
            dependency_graph_indices: HashMap::default(),
            id_to_location: HashMap::default(),
            definitions: vec![],
            id_to_type: HashMap::default(),
            definition_to_type: HashMap::default(),
            data_types: HashMap::default(),
            type_attributes: HashMap::default(),
            type_aliases: Vec::new(),
            trait_associated_types: Vec::new(),
            traits: HashMap::default(),
            trait_implementations: HashMap::default(),
            next_trait_implementation_id: 0,
            trait_implementation_map: HashMap::default(),
            selected_trait_implementations: HashMap::default(),
            infix_operator_traits: HashMap::default(),
            prefix_operator_traits: HashMap::default(),
            ordering_type: None,
            instantiation_bindings: HashMap::default(),
            field_indices: HashMap::default(),
            next_type_variable_id: std::cell::Cell::new(0),
            globals: Vec::new(),
            global_attributes: HashMap::default(),
            methods: HashMap::default(),
            type_alias_ref: Vec::new(),
            type_ref_locations: Vec::new(),
            quoted_types: Default::default(),
            interned_expression_kinds: Default::default(),
            interned_statement_kinds: Default::default(),
            interned_unresolved_type_data: Default::default(),
            interned_patterns: Default::default(),
            lsp_mode: false,
            location_indices: LocationIndices::default(),
            reference_graph: petgraph::graph::DiGraph::new(),
            reference_graph_indices: HashMap::default(),
            auto_import_names: HashMap::default(),
            comptime_scopes: vec![HashMap::default()],
            trait_impl_associated_types: HashMap::default(),
            trait_impl_associated_constants: HashMap::default(),
            doc_comments: HashMap::default(),
            reexports: HashMap::default(),
        }
    }
}

// XXX: Add check that insertions are not overwrites for maps
// XXX: Maybe change push to intern, and remove comments
impl NodeInterner {
    /// Interns a HIR statement.
    pub fn push_stmt(&mut self, stmt: HirStatement) -> StmtId {
        StmtId(self.nodes.insert(Node::Statement(stmt)))
    }
    /// Interns a HIR expression.
    pub fn push_expr(&mut self, expr: HirExpression) -> ExprId {
        ExprId(self.nodes.insert(Node::Expression(expr)))
    }

    /// Intern an expression with everything needed for it (location & Type)
    /// instead of requiring they be pushed later.
    pub fn push_expr_full(&mut self, expr: HirExpression, location: Location, typ: Type) -> ExprId {
        let id = self.push_expr(expr);
        self.push_expr_location(id, location);
        self.push_expr_type(id, typ);
        id
    }

    /// Stores the span for an interned expression.
    pub fn push_expr_location(&mut self, expr_id: ExprId, location: Location) {
        self.id_to_location.insert(expr_id.into(), location);
    }

    /// Interns a HIR Function.
    pub fn push_fn(&mut self, func: HirFunction) -> FuncId {
        FuncId(self.nodes.insert(Node::Function(func)))
    }

    /// Store the type for an interned expression
    pub fn push_expr_type(&mut self, expr_id: ExprId, typ: Type) {
        self.id_to_type.insert(expr_id.into(), typ);
    }

    /// Store the type for a definition
    pub fn push_definition_type(&mut self, definition_id: DefinitionId, typ: Type) {
        self.definition_to_type.insert(definition_id, typ);
    }

    pub fn push_empty_trait(
        &mut self,
        type_id: TraitId,
        unresolved_trait: &UnresolvedTrait,
        generics: Generics,
        associated_types: Generics,
        associated_constant_ids: HashMap<String, DefinitionId>,
    ) {
        let new_trait = Trait {
            id: type_id,
            name: unresolved_trait.trait_def.name.clone(),
            crate_id: unresolved_trait.crate_id,
            location: unresolved_trait.trait_def.name.location(),
            generics,
            visibility: ItemVisibility::Private,
            self_type_typevar: TypeVariable::unbound(self.next_type_variable_id(), Kind::Normal),
            methods: Vec::new(),
            method_ids: unresolved_trait.method_ids.clone(),
            associated_types,
            associated_type_bounds: HashMap::default(),
            trait_bounds: Vec::new(),
            where_clause: Vec::new(),
            all_generics: Vec::new(),
            associated_constant_ids,
        };

        self.traits.insert(type_id, new_trait);
    }

    /// Creates a new struct or enum type with no fields or variants.
    #[allow(clippy::too_many_arguments)]
    pub fn new_type(
        &mut self,
        name: Ident,
        span: Span,
        attributes: Vec<SecondaryAttribute>,
        generics: Generics,
        visibility: ItemVisibility,
        krate: CrateId,
        local_id: LocalModuleId,
        file_id: FileId,
    ) -> TypeId {
        let type_id = TypeId(ModuleId { krate, local_id });

        let location = Location::new(span, file_id);
        let new_type = DataType::new(type_id, name, location, generics, visibility);
        self.data_types.insert(type_id, Shared::new(new_type));
        self.type_attributes.insert(type_id, attributes);
        type_id
    }

    pub fn push_type_alias(
        &mut self,
        typ: &UnresolvedTypeAlias,
        generics: Generics,
    ) -> TypeAliasId {
        let type_id = TypeAliasId(self.type_aliases.len());

        self.type_aliases.push(Shared::new(TypeAlias::new(
            type_id,
            typ.type_alias_def.name.clone(),
            typ.type_alias_def.location,
            Type::Error,
            generics,
            typ.type_alias_def.visibility,
            ModuleId { krate: typ.crate_id, local_id: typ.module_id },
        )));

        type_id
    }

    /// Adds [TypeAliasId] and [Location] to the type_alias_ref vector
    /// So that we can later resolve [Location]s type aliases from the LSP requests
    pub fn add_type_alias_ref(&mut self, type_id: TypeAliasId, location: Location) {
        self.type_alias_ref.push((type_id, location));
    }

    pub fn push_trait_associated_type(
        &mut self,
        trait_id: TraitId,
        name: Ident,
    ) -> TraitAssociatedTypeId {
        let id = TraitAssociatedTypeId(self.trait_associated_types.len());
        self.trait_associated_types.push(TraitAssociatedType { id, trait_id, name });
        id
    }

    pub fn update_type(&mut self, type_id: TypeId, f: impl FnOnce(&mut DataType)) {
        let mut value = self.data_types.get_mut(&type_id).unwrap().borrow_mut();
        f(&mut value);
    }

    pub fn update_trait(&mut self, trait_id: TraitId, f: impl FnOnce(&mut Trait)) {
        let value = self.traits.get_mut(&trait_id).unwrap();
        f(value);
    }

    pub fn update_type_attributes(&mut self, type_id: TypeId, f: impl FnOnce(&mut TypeAttributes)) {
        let value = self.type_attributes.get_mut(&type_id).unwrap();
        f(value);
    }

    pub fn set_type_alias(&mut self, type_id: TypeAliasId, typ: Type, generics: Generics) {
        let type_alias_type = &mut self.type_aliases[type_id.0];
        type_alias_type.borrow_mut().set_type_and_generics(typ, generics);
    }

    /// Returns the interned statement corresponding to `stmt_id`
    pub fn update_statement(&mut self, stmt_id: &StmtId, f: impl FnOnce(&mut HirStatement)) {
        let def =
            self.nodes.get_mut(stmt_id.0).expect("ice: all statement ids should have definitions");

        match def {
            Node::Statement(stmt) => f(stmt),
            _ => panic!("ice: all statement ids should correspond to a statement in the interner"),
        }
    }

    /// Updates the interned expression corresponding to `expr_id`
    pub fn update_expression(&mut self, expr_id: ExprId, f: impl FnOnce(&mut HirExpression)) {
        let def =
            self.nodes.get_mut(expr_id.0).expect("ice: all expression ids should have definitions");

        match def {
            Node::Expression(expr) => f(expr),
            _ => {
                panic!("ice: all expression ids should correspond to a expression in the interner")
            }
        }
    }

    /// Store [Location] of [Type] reference
    pub fn push_type_ref_location(&mut self, typ: &Type, location: Location) {
        if !self.is_in_lsp_mode() {
            return;
        }

        self.type_ref_locations.push((typ.clone(), location));
    }

    #[allow(clippy::too_many_arguments)]
    fn push_global(
        &mut self,
        ident: Ident,
        local_id: LocalModuleId,
        crate_id: CrateId,
        let_statement: StmtId,
        file: FileId,
        attributes: Vec<SecondaryAttribute>,
        mutable: bool,
        comptime: bool,
        visibility: ItemVisibility,
    ) -> GlobalId {
        let id = GlobalId(self.globals.len());
        let location = Location::new(ident.span(), file);
        let name = ident.to_string();

        let definition_id =
            self.push_definition(name, mutable, comptime, DefinitionKind::Global(id), location);

        self.globals.push(GlobalInfo {
            id,
            definition_id,
            ident,
            local_id,
            crate_id,
            let_statement,
            location,
            visibility,
            value: GlobalValue::Unresolved,
        });
        self.global_attributes.insert(id, attributes);
        id
    }

    pub fn next_global_id(&mut self) -> GlobalId {
        GlobalId(self.globals.len())
    }

    /// Intern an empty global. Used for collecting globals before they're defined
    #[allow(clippy::too_many_arguments)]
    pub fn push_empty_global(
        &mut self,
        name: Ident,
        local_id: LocalModuleId,
        crate_id: CrateId,
        file: FileId,
        attributes: Vec<SecondaryAttribute>,
        mutable: bool,
        comptime: bool,
        visibility: ItemVisibility,
    ) -> GlobalId {
        let statement = self.push_stmt(HirStatement::Error);
        let location = name.location();

        let id = self.push_global(
            name, local_id, crate_id, statement, file, attributes, mutable, comptime, visibility,
        );
        self.push_stmt_location(statement, location);
        id
    }

    /// Intern an empty function.
    pub fn push_empty_fn(&mut self) -> FuncId {
        self.push_fn(HirFunction::empty())
    }
    /// Updates the underlying interned Function.
    ///
    /// This method is used as we eagerly intern empty functions to
    /// generate function identifiers and then we update at a later point in
    /// time.
    pub fn update_fn(&mut self, func_id: FuncId, hir_func: HirFunction) {
        let def =
            self.nodes.get_mut(func_id.0).expect("ice: all function ids should have definitions");

        let func = match def {
            Node::Function(func) => func,
            _ => panic!("ice: all function ids should correspond to a function in the interner"),
        };
        *func = hir_func;
    }

    pub fn find_function(&self, function_name: &str) -> Option<FuncId> {
        self.func_meta
            .iter()
            .find(|(func_id, _func_meta)| self.function_name(func_id) == function_name)
            .map(|(func_id, _meta)| *func_id)
    }

    ///Interns a function's metadata.
    ///
    /// Note that the FuncId has been created already.
    /// See ModCollector for it's usage.
    pub fn push_fn_meta(&mut self, func_data: FuncMeta, func_id: FuncId) {
        self.func_meta.insert(func_id, func_data);
    }

    pub fn definition_count(&self) -> usize {
        self.definitions.len()
    }

    pub fn push_definition(
        &mut self,
        name: String,
        mutable: bool,
        comptime: bool,
        definition: DefinitionKind,
        location: Location,
    ) -> DefinitionId {
        let id = DefinitionId(self.definitions.len());
        let is_local = matches!(definition, DefinitionKind::Local(_));

        if let DefinitionKind::Function(func_id) = definition {
            self.function_definition_ids.insert(func_id, id);
        }

        let kind = definition;
        self.definitions.push(DefinitionInfo { name, mutable, comptime, kind, location });

        if is_local {
            self.add_definition_location(ReferenceId::Local(id), location);
        }

        id
    }

    /// Push a function with the default modifiers and [`ModuleId`] for testing
    #[cfg(test)]
    pub fn push_test_function_definition(&mut self, name: String) -> FuncId {
        let id = self.push_fn(HirFunction::empty());
        let mut modifiers = FunctionModifiers::new();
        modifiers.name = name;
        let module = ModuleId::dummy_id();
        let location = Location::dummy();
        self.push_function_definition(id, modifiers, module, location);
        id
    }

    pub fn push_function(
        &mut self,
        id: FuncId,
        function: &FunctionDefinition,
        module: ModuleId,
        location: Location,
    ) -> DefinitionId {
        let name_location = Location::new(function.name.span(), location.file);
        let modifiers = FunctionModifiers {
            name: function.name.to_string(),
            visibility: function.visibility,
            attributes: function.attributes.clone(),
            is_unconstrained: function.is_unconstrained,
            generic_count: function.generics.len(),
            is_comptime: function.is_comptime,
            name_location,
        };
        let definition_id = self.push_function_definition(id, modifiers, module, location);
        self.add_definition_location(ReferenceId::Function(id), name_location);
        definition_id
    }

    pub fn push_function_definition(
        &mut self,
        func: FuncId,
        modifiers: FunctionModifiers,
        module: ModuleId,
        location: Location,
    ) -> DefinitionId {
        let name = modifiers.name.clone();
        let comptime = modifiers.is_comptime;
        self.function_modifiers.insert(func, modifiers);
        self.function_modules.insert(func, module);
        self.push_definition(name, false, comptime, DefinitionKind::Function(func), location)
    }

    pub fn set_function_trait(&mut self, func: FuncId, self_type: Type, trait_id: TraitId) {
        self.func_id_to_trait.insert(func, (self_type, trait_id));
    }

    pub fn get_function_trait(&self, func: &FuncId) -> Option<(Type, TraitId)> {
        self.func_id_to_trait.get(func).cloned()
    }

    /// Returns the visibility of the given function.
    ///
    /// The underlying function_visibilities map is populated during def collection,
    /// so this function can be called anytime afterward.
    pub fn function_visibility(&self, func: FuncId) -> ItemVisibility {
        self.function_modifiers[&func].visibility
    }

    /// Returns the module this function was defined within
    pub fn function_module(&self, func: FuncId) -> ModuleId {
        self.function_modules[&func]
    }

    /// Returns the [`FuncId`] corresponding to the function referred to by `expr_id`
    pub fn lookup_function_from_expr(&self, expr: &ExprId) -> Option<FuncId> {
        if let HirExpression::Ident(HirIdent { id, .. }, _) = self.expression(expr) {
            match self.try_definition(id).map(|def| &def.kind) {
                Some(DefinitionKind::Function(func_id)) => Some(*func_id),
                Some(DefinitionKind::Local(Some(expr_id))) => {
                    self.lookup_function_from_expr(expr_id)
                }
                _ => None,
            }
        } else {
            None
        }
    }

    /// Returns the interned HIR function corresponding to `func_id`
    //
    // Cloning HIR structures is cheap, so we return owned structures
    pub fn function(&self, func_id: &FuncId) -> HirFunction {
        let def = self.nodes.get(func_id.0).expect("ice: all function ids should have definitions");

        match def {
            Node::Function(func) => func.clone(),
            _ => panic!("ice: all function ids should correspond to a function in the interner"),
        }
    }

    /// Returns the interned meta data corresponding to `func_id`
    pub fn function_meta(&self, func_id: &FuncId) -> &FuncMeta {
        self.func_meta.get(func_id).expect("ice: all function ids should have metadata")
    }

    pub fn function_meta_mut(&mut self, func_id: &FuncId) -> &mut FuncMeta {
        self.func_meta.get_mut(func_id).expect("ice: all function ids should have metadata")
    }

    pub fn try_function_meta(&self, func_id: &FuncId) -> Option<&FuncMeta> {
        self.func_meta.get(func_id)
    }

    pub fn function_ident(&self, func_id: &FuncId) -> crate::ast::Ident {
        let name = self.function_name(func_id).to_owned();
        let location = self.function_meta(func_id).name.location;
        crate::ast::Ident::new(name, location)
    }

    pub fn function_name(&self, func_id: &FuncId) -> &str {
        &self.function_modifiers[func_id].name
    }

    pub fn function_modifiers(&self, func_id: &FuncId) -> &FunctionModifiers {
        &self.function_modifiers[func_id]
    }

    pub fn function_modifiers_mut(&mut self, func_id: &FuncId) -> &mut FunctionModifiers {
        self.function_modifiers.get_mut(func_id).expect("func_id should always have modifiers")
    }

    pub fn function_attributes(&self, func_id: &FuncId) -> &Attributes {
        &self.function_modifiers[func_id].attributes
    }

    pub fn type_attributes(&self, struct_id: &TypeId) -> &TypeAttributes {
        &self.type_attributes[struct_id]
    }

    pub fn add_module_attributes(&mut self, module_id: ModuleId, attributes: ModuleAttributes) {
        self.module_attributes.insert(module_id, attributes);
    }

    pub fn module_attributes(&self, module_id: ModuleId) -> &ModuleAttributes {
        &self.module_attributes[&module_id]
    }

    pub fn try_module_attributes(&self, module_id: ModuleId) -> Option<&ModuleAttributes> {
        self.module_attributes.get(&module_id)
    }

    pub fn global_attributes(&self, global_id: &GlobalId) -> &[SecondaryAttribute] {
        &self.global_attributes[global_id]
    }

    /// Returns the interned statement corresponding to `stmt_id`
    pub fn statement(&self, stmt_id: &StmtId) -> HirStatement {
        let def =
            self.nodes.get(stmt_id.0).expect("ice: all statement ids should have definitions");

        match def {
            Node::Statement(stmt) => stmt.clone(),
            _ => panic!("ice: all statement ids should correspond to a statement in the interner"),
        }
    }

    /// Try to get the `HirLetStatement` which defines a given global value
    pub fn get_global_let_statement(&self, global: GlobalId) -> Option<HirLetStatement> {
        let global = self.get_global(global);
        let def = self.nodes.get(global.let_statement.0)?;

        match def {
            Node::Statement(hir_stmt) => match hir_stmt {
                HirStatement::Let(let_stmt) => Some(let_stmt.clone()),
                HirStatement::Error => None,
                other => {
                    panic!(
                        "ice: all globals should correspond to a let statement in the interner: {other:?}"
                    )
                }
            },
            _ => panic!("ice: all globals should correspond to a statement in the interner"),
        }
    }

    /// Returns the interned expression corresponding to `expr_id`
    pub fn expression(&self, expr_id: &ExprId) -> HirExpression {
        let def =
            self.nodes.get(expr_id.0).expect("ice: all expression ids should have definitions");

        match def {
            Node::Expression(expr) => expr.clone(),
            _ => {
                panic!("ice: all expression ids should correspond to a expression in the interner")
            }
        }
    }

    /// Retrieves the definition where the given id was defined.
    /// This will panic if given DefinitionId::dummy_id. Use try_definition for
    /// any call with a possibly undefined variable.
    pub fn definition(&self, id: DefinitionId) -> &DefinitionInfo {
        &self.definitions[id.0]
    }

    /// Retrieves the definition where the given id was defined.
    /// This will panic if given DefinitionId::dummy_id. Use try_definition for
    /// any call with a possibly undefined variable.
    pub fn definition_mut(&mut self, id: DefinitionId) -> &mut DefinitionInfo {
        &mut self.definitions[id.0]
    }

    /// Tries to retrieve the given id's definition.
    /// This function should be used during name resolution or type checking when we cannot be sure
    /// all variables have corresponding definitions (in case of an error in the user's code).
    pub fn try_definition(&self, id: DefinitionId) -> Option<&DefinitionInfo> {
        self.definitions.get(id.0)
    }

    /// Returns the name of the definition
    ///
    /// This is needed as the Environment needs to map variable names to witness indices
    pub fn definition_name(&self, id: DefinitionId) -> &str {
        &self.definition(id).name
    }

    pub fn expr_span(&self, expr_id: &ExprId) -> Span {
        self.id_location(expr_id).span
    }

    pub fn try_expr_span(&self, expr_id: &ExprId) -> Option<Span> {
        self.try_id_location(expr_id).map(|location| location.span)
    }

    pub fn expr_location(&self, expr_id: &ExprId) -> Location {
        self.id_location(expr_id)
    }

    pub fn statement_span(&self, stmt_id: StmtId) -> Span {
        self.id_location(stmt_id).span
    }

    pub fn statement_location(&self, stmt_id: StmtId) -> Location {
        self.id_location(stmt_id)
    }

    pub fn push_stmt_location(&mut self, id: StmtId, location: Location) {
        self.id_to_location.insert(id.into(), location);
    }

    pub fn get_type(&self, id: TypeId) -> Shared<DataType> {
        self.data_types[&id].clone()
    }

    pub fn get_type_methods(&self, typ: &Type) -> Option<&HashMap<String, Methods>> {
        get_type_method_key(typ).and_then(|key| self.methods.get(&key))
    }

    pub fn get_trait(&self, id: TraitId) -> &Trait {
        &self.traits[&id]
    }

    pub fn get_trait_associated_type(&self, id: TraitAssociatedTypeId) -> &TraitAssociatedType {
        &self.trait_associated_types[id.0]
    }

    pub fn get_trait_mut(&mut self, id: TraitId) -> &mut Trait {
        self.traits.get_mut(&id).expect("get_trait_mut given invalid TraitId")
    }

    pub fn try_get_trait(&self, id: TraitId) -> Option<&Trait> {
        self.traits.get(&id)
    }

    pub fn get_type_alias(&self, id: TypeAliasId) -> Shared<TypeAlias> {
        self.type_aliases[id.0].clone()
    }

    pub fn get_global(&self, global_id: GlobalId) -> &GlobalInfo {
        &self.globals[global_id.0]
    }

    pub fn get_global_mut(&mut self, global_id: GlobalId) -> &mut GlobalInfo {
        &mut self.globals[global_id.0]
    }

    pub fn get_global_definition(&self, global_id: GlobalId) -> &DefinitionInfo {
        let global = self.get_global(global_id);
        self.definition(global.definition_id)
    }

    pub fn get_global_definition_mut(&mut self, global_id: GlobalId) -> &mut DefinitionInfo {
        let global = self.get_global(global_id);
        self.definition_mut(global.definition_id)
    }

    pub fn get_all_globals(&self) -> &[GlobalInfo] {
        &self.globals
    }

    /// Returns the type of an item stored in the Interner or Error if it was not found.
    pub fn id_type(&self, index: impl Into<Index>) -> Type {
        self.try_id_type(index).cloned().unwrap_or(Type::Error)
    }

    pub fn try_id_type(&self, index: impl Into<Index>) -> Option<&Type> {
        self.id_to_type.get(&index.into())
    }

    /// Returns the type of the definition or `Type::Error` if it was not found.
    pub fn definition_type(&self, id: DefinitionId) -> Type {
        self.definition_to_type.get(&id).cloned().unwrap_or(Type::Error)
    }

    pub fn id_type_substitute_trait_as_type(&self, def_id: DefinitionId) -> Type {
        let typ = self.definition_type(def_id);
        if let Type::Function(args, ret, env, unconstrained) = &typ {
            let def = self.definition(def_id);
            if let Type::TraitAsType(..) = ret.as_ref() {
                if let DefinitionKind::Function(func_id) = def.kind {
                    let f = self.function(&func_id);
                    let func_body = f.as_expr();
                    let ret_type = self.id_type(func_body);
                    let new_type = Type::Function(
                        args.clone(),
                        Box::new(ret_type),
                        env.clone(),
                        *unconstrained,
                    );
                    return new_type;
                }
            }
        }
        typ
    }

    /// Returns the span of an item stored in the Interner
    pub fn id_location(&self, index: impl Into<Index> + Copy) -> Location {
        self.try_id_location(index)
            .unwrap_or_else(|| panic!("ID is missing a source location: {:?}", index.into()))
    }

    /// Returns the span of an item stored in the Interner, if present
    pub fn try_id_location(&self, index: impl Into<Index>) -> Option<Location> {
        self.id_to_location.get(&index.into()).copied()
    }

    /// Replaces the HirExpression at the given ExprId with a new HirExpression
    pub fn replace_expr(&mut self, id: &ExprId, new: HirExpression) {
        let old = self.nodes.get_mut(id.into()).unwrap();
        *old = Node::Expression(new);
    }

    /// Replaces the HirStatement at the given StmtId with a new HirStatement
    pub fn replace_statement(&mut self, stmt_id: StmtId, hir_stmt: HirStatement) {
        let old = self.nodes.get_mut(stmt_id.0).unwrap();
        *old = Node::Statement(hir_stmt);
    }

    pub fn next_type_variable_id(&self) -> TypeVariableId {
        let id = self.next_type_variable_id.get();
        self.next_type_variable_id.set(id + 1);
        TypeVariableId(id)
    }

    pub fn next_type_variable(&self) -> Type {
        Type::type_variable(self.next_type_variable_id())
    }

    pub fn next_type_variable_with_kind(&self, kind: Kind) -> Type {
        Type::type_variable_with_kind(self, kind)
    }

    pub fn store_instantiation_bindings(
        &mut self,
        expr_id: ExprId,
        instantiation_bindings: TypeBindings,
    ) {
        self.instantiation_bindings.insert(expr_id, instantiation_bindings);
    }

    pub fn get_instantiation_bindings(&self, expr_id: ExprId) -> &TypeBindings {
        &self.instantiation_bindings[&expr_id]
    }

    pub fn try_get_instantiation_bindings(&self, expr_id: ExprId) -> Option<&TypeBindings> {
        self.instantiation_bindings.get(&expr_id)
    }

    pub fn get_field_index(&self, expr_id: ExprId) -> usize {
        self.field_indices[&expr_id]
    }

    pub fn set_field_index(&mut self, expr_id: ExprId, index: usize) {
        self.field_indices.insert(expr_id, index);
    }

    pub fn function_definition_id(&self, function: FuncId) -> DefinitionId {
        self.function_definition_ids[&function]
    }

    /// Returns the definition id and trait id for a given trait or impl function.
    ///
    /// If this is an impl function, the DefinitionId inside the TraitItemId will still
    /// be that of the function in the parent trait.
    pub fn get_trait_item_id(&self, function_id: FuncId) -> Option<TraitItemId> {
        let function = self.function_meta(&function_id);

        match function.trait_impl {
            Some(impl_id) => {
                let trait_id = self.get_trait_implementation(impl_id).borrow().trait_id;
                let the_trait = self.get_trait(trait_id);
                let name = self.definition_name(function.name.id);
                let definition_id = the_trait
                    .find_method_or_constant(name, self)
                    .expect("Expected parent trait to have function from impl");
                Some(TraitItemId { item_id: definition_id, trait_id })
            }
            None => {
                let trait_id = function.trait_id?;
                let definition_id = self.function_definition_id(function_id);
                Some(TraitItemId { item_id: definition_id, trait_id })
            }
        }
    }

    /// Adds a non-trait method to a type.
    ///
    /// Returns `Some(duplicate)` if a matching method was already defined.
    /// Returns `None` otherwise.
    pub fn add_method(
        &mut self,
        self_type: &Type,
        method_name: String,
        method_id: FuncId,
        trait_id: Option<TraitId>,
    ) -> Option<FuncId> {
        match self_type {
            Type::Error => None,
            Type::Reference(element, _mutable) => {
                self.add_method(element, method_name, method_id, trait_id)
            }
            _ => {
                let key = get_type_method_key(self_type).unwrap_or_else(|| {
                    unreachable!("Cannot add a method to the unsupported type '{}'", self_type)
                });

                if trait_id.is_none() && matches!(self_type, Type::DataType(..)) {
                    let check_self_param = false;
                    if let Some(existing) =
                        self.lookup_direct_method(self_type, &method_name, check_self_param)
                    {
                        return Some(existing);
                    }
                }

                // Only remember the actual type if it's FieldOrInt,
                // so later we can disambiguate on calls like `u32::call`.
                let typ = self_type.clone();
                self.methods
                    .entry(key)
                    .or_default()
                    .entry(method_name)
                    .or_default()
                    .add_method(method_id, typ, trait_id);
                None
            }
        }
    }

    pub fn try_get_trait_implementation(&self, id: TraitImplId) -> Option<Shared<TraitImpl>> {
        self.trait_implementations.get(&id).cloned()
    }

    pub fn get_trait_implementation(&self, id: TraitImplId) -> Shared<TraitImpl> {
        self.trait_implementations[&id].clone()
    }

    pub fn get_trait_implementations_in_crate(&self, crate_id: CrateId) -> HashSet<TraitImplId> {
        let trait_impls = self.trait_implementations.iter();
        let trait_impls = trait_impls.filter_map(|(id, trait_impl)| {
            if trait_impl.borrow().crate_id == crate_id { Some(*id) } else { None }
        });
        trait_impls.collect()
    }

    #[allow(unused)]
    pub fn trait_constraint_string(
        &self,
        object_type: &Type,
        trait_id: TraitId,
        trait_generics: &[Type],
        trait_associated_types: &[NamedType],
    ) -> String {
        let name = self.get_trait(trait_id).name.to_string();
        let mut generics = vecmap(trait_generics, |t| format!("{t:?}")).join(", ");
        let associated =
            vecmap(trait_associated_types, |t| format!("{}: {:?}", t.name, t.typ)).join(", ");

        if !generics.is_empty() && !associated.is_empty() {
            generics += ", ";
            generics += &associated;
        }

        if !generics.is_empty() {
            generics = format!("<{generics}>");
        }
        format!("{object_type:?}: {name}{generics}")
    }

    /// Given a `ObjectType: TraitId` pair, try to find an existing impl that satisfies the
    /// constraint. If an impl cannot be found, this will return a vector of each constraint
    /// in the path to get to the failing constraint. Usually this is just the single failing
    /// constraint, but when where clauses are involved, the failing constraint may be several
    /// constraints deep. In this case, all of the constraints are returned, starting with the
    /// failing one.
    /// If this list of failing constraints is empty, this means type annotations are required.
    /// Returns the list of instantiation bindings as well, which should be stored on the
    /// expression.
    pub fn lookup_trait_implementation(
        &self,
        object_type: &Type,
        trait_id: TraitId,
        trait_generics: &[Type],
        trait_associated_types: &[NamedType],
    ) -> Result<(TraitImplKind, TypeBindings), ImplSearchErrorKind> {
        let (impl_kind, bindings, instantiation_bindings) = self.try_lookup_trait_implementation(
            object_type,
            trait_id,
            trait_generics,
            trait_associated_types,
        )?;

        Type::apply_type_bindings(bindings);
        Ok((impl_kind, instantiation_bindings))
    }

    /// Similar to `lookup_trait_implementation` but does not apply any type bindings on success.
    /// On error returns either:
    /// - 1+ failing trait constraints, including the original.
    ///   Each constraint after the first represents a `where` clause that was followed.
    /// - 0 trait constraints indicating type annotations are needed to choose an impl.
    pub fn try_lookup_trait_implementation(
        &self,
        object_type: &Type,
        trait_id: TraitId,
        trait_generics: &[Type],
        trait_associated_types: &[NamedType],
    ) -> Result<(TraitImplKind, TypeBindings, TypeBindings), ImplSearchErrorKind> {
        let mut bindings = TypeBindings::default();
        let (impl_kind, instantiation_bindings) = self.lookup_trait_implementation_helper(
            object_type,
            trait_id,
            trait_generics,
            trait_associated_types,
            &mut bindings,
            IMPL_SEARCH_RECURSION_LIMIT,
        )?;
        Ok((impl_kind, bindings, instantiation_bindings))
    }

    /// Returns the trait implementation if found along with the instantiation bindings for
    /// instantiating that trait impl. Note that this is separate from the passed-in TypeBindings
    /// which can be bound via `Type::apply_type_bindings` if needed. Instantiation bindings should
    /// be stored as such but not bound, lest the original named generics in trait impls get bound
    /// over.
    ///
    /// On error returns either:
    /// - 1+ failing trait constraints, including the original.
    ///   Each constraint after the first represents a `where` clause that was followed.
    /// - 0 trait constraints indicating type annotations are needed to choose an impl.
    fn lookup_trait_implementation_helper(
        &self,
        object_type: &Type,
        trait_id: TraitId,
        trait_generics: &[Type],
        trait_associated_types: &[NamedType],
        type_bindings: &mut TypeBindings,
        recursion_limit: u32,
    ) -> Result<(TraitImplKind, TypeBindings), ImplSearchErrorKind> {
        let make_constraint = || {
            let ordered = trait_generics.to_vec();
            let named = trait_associated_types.to_vec();
            TraitConstraint {
                typ: object_type.clone(),
                trait_bound: ResolvedTraitBound {
                    trait_id,
                    trait_generics: TraitGenerics { ordered, named },
                    location: Location::dummy(),
                },
            }
        };

        let nested_error = || ImplSearchErrorKind::Nested(vec![make_constraint()]);

        // Prevent infinite recursion when looking for impls
        if recursion_limit == 0 {
            return Err(nested_error());
        }

        let object_type = object_type.substitute(type_bindings);

        // If the object type isn't known, just return an error saying type annotations are needed.
        if object_type.is_bindable() {
            return Err(ImplSearchErrorKind::TypeAnnotationsNeededOnObjectType);
        }

        let impls = self.trait_implementation_map.get(&trait_id).ok_or_else(nested_error)?;

        let mut matching_impls = Vec::new();
        let mut where_clause_error = None;

        for (existing_object_type, impl_kind) in impls {
            let (existing_object_type, instantiation_bindings) =
                existing_object_type.instantiate(self);

            let mut fresh_bindings = type_bindings.clone();

            if object_type.try_unify(&existing_object_type, &mut fresh_bindings).is_err() {
                continue;
            }

            let impl_trait_generics = match impl_kind {
                TraitImplKind::Normal(id) => {
                    let shared_impl = self.get_trait_implementation(*id);
                    let shared_impl = shared_impl.borrow();
                    let named = self.get_associated_types_for_impl(*id).to_vec();
                    let ordered = shared_impl.trait_generics.clone();
                    TraitGenerics { named, ordered }
                }
                TraitImplKind::Assumed { trait_generics, .. } => trait_generics.clone(),
            };

            let generics_unify = trait_generics.iter().zip(&impl_trait_generics.ordered).all(
                |(trait_generic, impl_generic)| {
                    let impl_generic = impl_generic.force_substitute(&instantiation_bindings);
                    trait_generic.try_unify(&impl_generic, &mut fresh_bindings).is_ok()
                },
            );

            if !generics_unify {
                continue;
            }

            if let TraitImplKind::Normal(impl_id) = impl_kind {
                let trait_impl = self.get_trait_implementation(*impl_id);
                let trait_impl = trait_impl.borrow();

                if let Err(error) = self.validate_where_clause(
                    &trait_impl.where_clause,
                    &mut fresh_bindings,
                    &instantiation_bindings,
                    recursion_limit,
                ) {
                    // Only keep the first errors we get from a failing where clause
                    if where_clause_error.is_none() {
                        where_clause_error = Some(error);
                    }
                    continue;
                }
            }

            let associated_types_unify = trait_associated_types
                .iter()
                .zip(&impl_trait_generics.named)
                .all(|(trait_generic, impl_generic)| {
                    let impl_generic2 = impl_generic.typ.force_substitute(&instantiation_bindings);
                    trait_generic.typ.try_unify(&impl_generic2, &mut fresh_bindings).is_ok()
                });
            if !associated_types_unify {
                continue;
            }

            let constraint = TraitConstraint {
                typ: existing_object_type,
                trait_bound: ResolvedTraitBound {
                    trait_id,
                    trait_generics: impl_trait_generics,
                    location: Location::dummy(),
                },
            };
            matching_impls.push((
                impl_kind.clone(),
                fresh_bindings,
                instantiation_bindings,
                constraint,
            ));
        }

        if matching_impls.len() == 1 {
            let (impl_, fresh_bindings, instantiation_bindings, _) = matching_impls.pop().unwrap();
            *type_bindings = fresh_bindings;
            Ok((impl_, instantiation_bindings))
        } else if matching_impls.is_empty() {
            let mut errors = match where_clause_error {
                Some((_, ImplSearchErrorKind::Nested(errors))) => errors,
                Some((constraint, _other)) => vec![constraint],
                None => vec![],
            };
            errors.push(make_constraint());
            Err(ImplSearchErrorKind::Nested(errors))
        } else {
            let impls = vecmap(matching_impls, |(_, _, _, constraint)| {
                let name = &self.get_trait(constraint.trait_bound.trait_id).name;
                format!("{}: {name}{}", constraint.typ, constraint.trait_bound.trait_generics)
            });
            Err(ImplSearchErrorKind::MultipleMatching(impls))
        }
    }

    /// Verifies that each constraint in the given where clause is valid.
    /// If an impl cannot be found for any constraint, the erroring constraint is returned.
    fn validate_where_clause(
        &self,
        where_clause: &[TraitConstraint],
        type_bindings: &mut TypeBindings,
        instantiation_bindings: &TypeBindings,
        recursion_limit: u32,
    ) -> Result<(), (TraitConstraint, ImplSearchErrorKind)> {
        for constraint in where_clause {
            // Instantiation bindings are generally safe to force substitute into the same type.
            // This is needed here to undo any bindings done to trait methods by monomorphization.
            // Otherwise, an impl for any (A, B) could get narrowed to only an impl for e.g. (u8, u16).
            let constraint_type =
                constraint.typ.force_substitute(instantiation_bindings).substitute(type_bindings);

            let trait_generics =
                vecmap(&constraint.trait_bound.trait_generics.ordered, |generic| {
                    generic.force_substitute(instantiation_bindings).substitute(type_bindings)
                });

            let trait_associated_types =
                vecmap(&constraint.trait_bound.trait_generics.named, |generic| {
                    let typ = generic.typ.force_substitute(instantiation_bindings);
                    NamedType { name: generic.name.clone(), typ: typ.substitute(type_bindings) }
                });

            // We can ignore any associated types on the constraint since those should not affect
            // which impl we choose.
            self.lookup_trait_implementation_helper(
                &constraint_type,
                constraint.trait_bound.trait_id,
                &trait_generics,
                &trait_associated_types,
                // Use a fresh set of type bindings here since the constraint_type originates from
                // our impl list, which we don't want to bind to.
                type_bindings,
                recursion_limit - 1,
            )
            .map_err(|error| (constraint.clone(), error))?;
        }

        Ok(())
    }

    /// Adds an "assumed" trait implementation to the currently known trait implementations.
    /// Unlike normal trait implementations, these are only assumed to exist. They often correspond
    /// to `where` clauses in functions where we assume there is some `T: Eq` even though we do
    /// not yet know T. For these cases, we store an impl here so that we assume they exist and
    /// can resolve them. They are then later verified when the function is called, and linked
    /// properly after being monomorphized to the correct variant.
    ///
    /// Returns true on success, or false if there is already an overlapping impl in scope.
    pub fn add_assumed_trait_implementation(
        &mut self,
        object_type: Type,
        trait_id: TraitId,
        trait_generics: TraitGenerics,
    ) -> bool {
        // Make sure there are no overlapping impls
        let existing = self.try_lookup_trait_implementation(
            &object_type,
            trait_id,
            &trait_generics.ordered,
            &trait_generics.named,
        );
        if existing.is_ok() {
            return false;
        }

        let entries = self.trait_implementation_map.entry(trait_id).or_default();
        entries.push((object_type.clone(), TraitImplKind::Assumed { object_type, trait_generics }));
        true
    }

    /// Adds a trait implementation to the list of known implementations.
    pub fn add_trait_implementation(
        &mut self,
        object_type: Type,
        trait_id: TraitId,
        impl_id: TraitImplId,
        impl_generics: GenericTypeVars,
        trait_impl: Shared<TraitImpl>,
    ) -> Result<(), Location> {
        self.trait_implementations.insert(impl_id, trait_impl.clone());

        // Avoid adding error types to impls since they'll conflict with every other type.
        // We don't need to return an error since we expect an error to already be issued when
        // the error type is created.
        if object_type == Type::Error {
            return Ok(());
        }

        // Replace each generic with a fresh type variable
        let substitutions = impl_generics
            .into_iter()
            .map(|typevar| {
                let typevar_kind = typevar.kind();
                let typevar_id = typevar.id();
                let substitution = (
                    typevar,
                    typevar_kind.clone(),
                    self.next_type_variable_with_kind(typevar_kind),
                );
                (typevar_id, substitution)
            })
            .collect();

        let instantiated_object_type = object_type.substitute(&substitutions);

        let trait_generics = &trait_impl.borrow().trait_generics;

        // Replace any associated types with fresh type variables so that we match
        // any existing impl regardless of associated types if one already exists.
        // E.g. if we already have an `impl Foo<Bar = i32> for Baz`, we should
        // reject `impl Foo<Bar = u32> for Baz` if it were to be added.
        let associated_types = self.get_associated_types_for_impl(impl_id);

        let associated_types = vecmap(associated_types, |named| {
            let typ = self.next_type_variable();
            NamedType { name: named.name.clone(), typ }
        });

        // Ignoring overlapping `TraitImplKind::Assumed` impls here is perfectly fine.
        // It should never happen since impls are defined at global scope, but even
        // if they were, we should never prevent defining a new impl because a 'where'
        // clause already assumes it exists.
        if let Ok((TraitImplKind::Normal(existing), ..)) = self.try_lookup_trait_implementation(
            &instantiated_object_type,
            trait_id,
            trait_generics,
            &associated_types,
        ) {
            let existing_impl = self.get_trait_implementation(existing);
            let existing_impl = existing_impl.borrow();
            return Err(existing_impl.ident.location());
        }

        for method in &trait_impl.borrow().methods {
            let method_name = self.function_name(method).to_owned();
            self.add_method(&object_type, method_name, *method, Some(trait_id));
        }

        // The object type is generalized so that a generic impl will apply
        // to any type T, rather than just the generic type named T.
        let generalized_object_type = object_type.generalize_from_substitutions(substitutions);

        let entries = self.trait_implementation_map.entry(trait_id).or_default();
        entries.push((generalized_object_type, TraitImplKind::Normal(impl_id)));
        Ok(())
    }

    /// Looks up a method that's directly defined in the given type.
    /// If `check_self_param` is `true`, only a method that has a `self` parameter with a type
    /// that unifies with `typ` will be returned.
    pub fn lookup_direct_method(
        &self,
        typ: &Type,
        method_name: &str,
        check_self_param: bool,
    ) -> Option<FuncId> {
        let key = get_type_method_key(typ)?;

        self.methods
            .get(&key)
            .and_then(|h| h.get(method_name))
            .and_then(|methods| methods.find_direct_method(typ, check_self_param, self))
    }

    /// Looks up a methods that apply to the given type but are defined in traits.
    pub fn lookup_trait_methods(
        &self,
        typ: &Type,
        method_name: &str,
        has_self_arg: bool,
    ) -> Vec<(FuncId, TraitId)> {
        let key = get_type_method_key(typ);
        if let Some(key) = key {
            self.methods
                .get(&key)
                .and_then(|h| h.get(method_name))
                .map(|methods| methods.find_trait_methods(typ, has_self_arg, self))
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Looks up methods at impls for all types `T`, e.g. `impl<T> Foo for T`
    pub fn lookup_generic_methods(
        &self,
        typ: &Type,
        method_name: &str,
        has_self_arg: bool,
    ) -> Vec<(FuncId, TraitId)> {
        self.methods
            .get(&TypeMethodKey::Generic)
            .and_then(|h| h.get(method_name))
            .map(|methods| methods.find_trait_methods(typ, has_self_arg, self))
            .unwrap_or_default()
    }

    /// Returns what the next trait impl id is expected to be.
    pub fn next_trait_impl_id(&mut self) -> TraitImplId {
        let next_id = self.next_trait_implementation_id;
        self.next_trait_implementation_id += 1;
        TraitImplId(next_id)
    }

    /// Removes all TraitImplKind::Assumed from the list of known impls for the given trait
    pub fn remove_assumed_trait_implementations_for_trait(&mut self, trait_id: TraitId) {
        self.remove_assumed_trait_implementations_for_trait_and_parents(trait_id, trait_id);
    }

    fn remove_assumed_trait_implementations_for_trait_and_parents(
        &mut self,
        trait_id: TraitId,
        starting_trait_id: TraitId,
    ) {
        let entries = self.trait_implementation_map.entry(trait_id).or_default();
        entries.retain(|(_, kind)| matches!(kind, TraitImplKind::Normal(_)));

        // Also remove assumed implementations for the parent traits, if any
        if let Some(trait_bounds) =
            self.try_get_trait(trait_id).map(|the_trait| the_trait.trait_bounds.clone())
        {
            for parent_trait_bound in trait_bounds {
                // Avoid looping forever in case there are cycles
                if parent_trait_bound.trait_id == starting_trait_id {
                    continue;
                }

                self.remove_assumed_trait_implementations_for_trait_and_parents(
                    parent_trait_bound.trait_id,
                    starting_trait_id,
                );
            }
        }
    }

    /// Tags the given identifier with the selected trait_impl so that monomorphization
    /// can later recover which impl was selected, or alternatively see if it needs to
    /// decide which impl to select (because the impl was Assumed).
    pub fn select_impl_for_expression(&mut self, ident_id: ExprId, trait_impl: TraitImplKind) {
        self.selected_trait_implementations.insert(ident_id, trait_impl);
    }

    /// Retrieves the impl selected for a given ExprId during name resolution.
    pub fn get_selected_impl_for_expression(&self, ident_id: ExprId) -> Option<TraitImplKind> {
        self.selected_trait_implementations.get(&ident_id).cloned()
    }

    /// Retrieves the trait id for a given binary operator.
    /// All binary operators correspond to a trait - although multiple may correspond
    /// to the same trait (such as `==` and `!=`).
    /// `self.infix_operator_traits` is expected to be filled before name resolution,
    /// during definition collection.
    pub fn get_operator_trait_method(&self, operator: BinaryOpKind) -> TraitItemId {
        let trait_id = self.infix_operator_traits[&operator];
        let the_trait = self.get_trait(trait_id);
        let func_id = *the_trait.method_ids.values().next().unwrap();
        TraitItemId { trait_id, item_id: self.function_definition_id(func_id) }
    }

    /// Retrieves the trait id for a given unary operator.
    /// Only some unary operators correspond to a trait: `-` and `!`, but for example `*` does not.
    /// `self.prefix_operator_traits` is expected to be filled before name resolution,
    /// during definition collection.
    pub fn get_prefix_operator_trait_method(&self, operator: &UnaryOp) -> Option<TraitItemId> {
        let trait_id = *self.prefix_operator_traits.get(operator)?;
        let the_trait = self.get_trait(trait_id);
        let func_id = *the_trait.method_ids.values().next().unwrap();
        Some(TraitItemId { trait_id, item_id: self.function_definition_id(func_id) })
    }

    /// Add the given trait as an operator trait if its name matches one of the
    /// operator trait names (Add, Sub, ...).
    pub fn try_add_infix_operator_trait(&mut self, trait_id: TraitId) {
        let the_trait = self.get_trait(trait_id);

        let operator = match the_trait.name.as_str() {
            "Add" => BinaryOpKind::Add,
            "Sub" => BinaryOpKind::Subtract,
            "Mul" => BinaryOpKind::Multiply,
            "Div" => BinaryOpKind::Divide,
            "Rem" => BinaryOpKind::Modulo,
            "Eq" => BinaryOpKind::Equal,
            "Ord" => BinaryOpKind::Less,
            "BitAnd" => BinaryOpKind::And,
            "BitOr" => BinaryOpKind::Or,
            "BitXor" => BinaryOpKind::Xor,
            "Shl" => BinaryOpKind::ShiftLeft,
            "Shr" => BinaryOpKind::ShiftRight,
            _ => return,
        };

        self.infix_operator_traits.insert(operator, trait_id);

        // Some operators also require we insert a matching entry for related operators
        match operator {
            BinaryOpKind::Equal => {
                self.infix_operator_traits.insert(BinaryOpKind::NotEqual, trait_id);
            }
            BinaryOpKind::Less => {
                self.infix_operator_traits.insert(BinaryOpKind::LessEqual, trait_id);
                self.infix_operator_traits.insert(BinaryOpKind::Greater, trait_id);
                self.infix_operator_traits.insert(BinaryOpKind::GreaterEqual, trait_id);

                let the_trait = self.get_trait(trait_id);
                self.ordering_type = match &the_trait.methods[0].typ {
                    Type::Forall(_, typ) => match typ.as_ref() {
                        Type::Function(_, return_type, _, _) => Some(return_type.as_ref().clone()),
                        other => unreachable!("Expected function type for `cmp`, found {}", other),
                    },
                    other => unreachable!("Expected Forall type for `cmp`, found {}", other),
                };
            }
            _ => (),
        }
    }

    /// Add the given trait as an operator trait if its name matches one of the
    /// prefix operator trait names (Not or Neg).
    pub fn try_add_prefix_operator_trait(&mut self, trait_id: TraitId) {
        let the_trait = self.get_trait(trait_id);

        let operator = match the_trait.name.as_str() {
            "Neg" => UnaryOp::Minus,
            "Not" => UnaryOp::Not,
            _ => return,
        };

        self.prefix_operator_traits.insert(operator, trait_id);
    }

    pub fn is_operator_trait(&self, trait_id: TraitId) -> bool {
        self.infix_operator_traits.values().any(|id| *id == trait_id)
            || self.prefix_operator_traits.values().any(|id| *id == trait_id)
    }

    /// This function is needed when creating a NodeInterner for testing so that calls
    /// to `get_operator_trait` do not panic when the stdlib isn't present.
    #[cfg(any(test, feature = "test_utils"))]
    pub fn populate_dummy_operator_traits(&mut self) {
        // Populate a dummy trait with a single method, as the trait, and its single methods,
        // are looked up during `get_operator_trait`.
        let mut usize_arena = Arena::default();
        let index = usize_arena.insert(0);
        let stdlib = CrateId::Stdlib(0);
        let func_id = FuncId::dummy_id();
        // Use a definition ID that won't clash with anything else, and isn't the dummy one
        let definition_id = DefinitionId(usize::MAX - 1);
        self.function_definition_ids.insert(func_id, definition_id);
        let module_id = ModuleId { krate: stdlib, local_id: LocalModuleId::new(index) };
        let trait_id = TraitId(module_id);
        let self_type_typevar = self.next_type_variable_id();
        let mut method_ids: HashMap<String, FuncId> = Default::default();
        method_ids.insert("dummy_method".to_string(), func_id);

        let trait_ = Trait {
            id: trait_id,
            crate_id: stdlib,
            methods: vec![],
            method_ids,
            associated_types: vec![],
            associated_type_bounds: Default::default(),
            name: Ident::new("Dummy".to_string(), Location::dummy()),
            generics: vec![],
            location: Location::dummy(),
            visibility: ItemVisibility::Public,
            self_type_typevar: TypeVariable::unbound(self_type_typevar, Kind::Normal),
            trait_bounds: vec![],
            where_clause: vec![],
            all_generics: vec![],
            associated_constant_ids: Default::default(),
        };
        self.traits.insert(trait_id, trait_);

        let operators = [
            BinaryOpKind::Add,
            BinaryOpKind::Subtract,
            BinaryOpKind::Multiply,
            BinaryOpKind::Divide,
            BinaryOpKind::Modulo,
            BinaryOpKind::Equal,
            BinaryOpKind::NotEqual,
            BinaryOpKind::Less,
            BinaryOpKind::LessEqual,
            BinaryOpKind::Greater,
            BinaryOpKind::GreaterEqual,
            BinaryOpKind::And,
            BinaryOpKind::Or,
            BinaryOpKind::Xor,
            BinaryOpKind::ShiftLeft,
            BinaryOpKind::ShiftRight,
        ];

        // It's fine to use the same trait for all operators, at least in tests
        for operator in operators {
            self.infix_operator_traits.insert(operator, trait_id);
        }
    }

    pub(crate) fn ordering_type(&self) -> Type {
        self.ordering_type.clone().expect("Expected ordering_type to be set in the NodeInterner")
    }

    /// Register that `dependent` depends on `dependency`.
    /// This is usually because `dependent` refers to `dependency` in one of its struct fields.
    pub fn add_type_dependency(&mut self, dependent: DependencyId, dependency: TypeId) {
        self.add_dependency(dependent, DependencyId::Struct(dependency));
    }

    pub fn add_global_dependency(&mut self, dependent: DependencyId, dependency: GlobalId) {
        self.add_dependency(dependent, DependencyId::Global(dependency));
    }

    pub fn add_function_dependency(&mut self, dependent: DependencyId, dependency: FuncId) {
        self.add_dependency(dependent, DependencyId::Function(dependency));
    }

    pub fn add_type_alias_dependency(&mut self, dependent: DependencyId, dependency: TypeAliasId) {
        self.add_dependency(dependent, DependencyId::Alias(dependency));
    }

    pub fn add_trait_dependency(&mut self, dependent: DependencyId, dependency: TraitId) {
        self.add_dependency(dependent, DependencyId::Trait(dependency));
    }

    pub fn add_dependency(&mut self, dependent: DependencyId, dependency: DependencyId) {
        let dependent_index = self.get_or_insert_dependency(dependent);
        let dependency_index = self.get_or_insert_dependency(dependency);
        self.dependency_graph.update_edge(dependent_index, dependency_index, ());
    }

    pub fn get_or_insert_dependency(&mut self, id: DependencyId) -> PetGraphIndex {
        if let Some(index) = self.dependency_graph_indices.get(&id) {
            return *index;
        }

        let index = self.dependency_graph.add_node(id);
        self.dependency_graph_indices.insert(id, index);
        index
    }

    pub(crate) fn check_for_dependency_cycles(&self) -> Vec<CompilationError> {
        let strongly_connected_components = tarjan_scc(&self.dependency_graph);
        let mut errors = Vec::new();

        let mut push_error = |item: String, scc: &[_], i, location: Location| {
            let cycle = self.get_cycle_error_string(scc, i);
            let error = ResolverError::DependencyCycle { item, cycle, location };
            errors.push(error.into());
        };

        for scc in strongly_connected_components {
            if scc.len() > 1 {
                // If a SCC contains a type, type alias, or global, it must be the only element in the SCC
                for (i, index) in scc.iter().enumerate() {
                    match self.dependency_graph[*index] {
                        DependencyId::Struct(struct_id) => {
                            let struct_type = self.get_type(struct_id);
                            let struct_type = struct_type.borrow();
                            push_error(struct_type.name.to_string(), &scc, i, struct_type.location);
                            break;
                        }
                        DependencyId::Global(global_id) => {
                            let global = self.get_global(global_id);
                            let name = global.ident.to_string();
                            push_error(name, &scc, i, global.location);
                            break;
                        }
                        DependencyId::Alias(alias_id) => {
                            let alias = self.get_type_alias(alias_id);
                            // If type aliases form a cycle, we have to manually break the cycle
                            // here to prevent infinite recursion in the type checker.
                            alias.borrow_mut().typ = Type::Error;

                            // push_error will borrow the alias so we have to drop the mutable borrow
                            let alias = alias.borrow();
                            push_error(alias.name.to_string(), &scc, i, alias.location);
                            break;
                        }
                        DependencyId::Trait(trait_id) => {
                            let the_trait = self.get_trait(trait_id);
                            push_error(the_trait.name.to_string(), &scc, i, the_trait.location);
                            break;
                        }
                        // Mutually recursive functions are allowed
                        DependencyId::Function(_) => (),
                        // Local variables should never be in a dependency cycle, scoping rules
                        // prevents referring to them before they're defined
                        DependencyId::Variable(loc) => unreachable!(
                            "Variable used at location {loc:?} caught in a dependency cycle"
                        ),
                    }
                }
            }
        }

        errors
    }

    /// Build up a string starting from the given item containing each item in the dependency
    /// cycle. The final result will resemble `foo -> bar -> baz -> foo`, always going back to the
    /// element at the given start index.
    fn get_cycle_error_string(&self, scc: &[PetGraphIndex], start_index: usize) -> String {
        let index_to_string = |index: PetGraphIndex| match self.dependency_graph[index] {
            DependencyId::Struct(id) => Cow::Owned(self.get_type(id).borrow().name.to_string()),
            DependencyId::Function(id) => Cow::Borrowed(self.function_name(&id)),
            DependencyId::Alias(id) => {
                Cow::Owned(self.get_type_alias(id).borrow().name.to_string())
            }
            DependencyId::Global(id) => Cow::Borrowed(self.get_global(id).ident.as_str()),
            DependencyId::Trait(id) => Cow::Owned(self.get_trait(id).name.to_string()),
            DependencyId::Variable(loc) => {
                unreachable!("Variable used at location {loc:?} caught in a dependency cycle")
            }
        };

        let mut cycle = index_to_string(scc[start_index]).to_string();

        // Reversing the dependencies here matches the order users would expect for the error message
        for i in (0..scc.len()).rev() {
            cycle += " -> ";
            cycle += &index_to_string(scc[(start_index + i) % scc.len()]);
        }

        cycle
    }

    pub fn push_quoted_type(&mut self, typ: Type) -> QuotedTypeId {
        QuotedTypeId(self.quoted_types.insert(typ))
    }

    pub fn get_quoted_type(&self, id: QuotedTypeId) -> &Type {
        &self.quoted_types[id.0]
    }

    pub fn push_expression_kind(&mut self, expr: ExpressionKind) -> InternedExpressionKind {
        InternedExpressionKind(self.interned_expression_kinds.insert(expr))
    }

    pub fn get_expression_kind(&self, id: InternedExpressionKind) -> &ExpressionKind {
        &self.interned_expression_kinds[id.0]
    }

    pub fn push_statement_kind(&mut self, statement: StatementKind) -> InternedStatementKind {
        InternedStatementKind(self.interned_statement_kinds.insert(statement))
    }

    pub fn get_statement_kind(&self, id: InternedStatementKind) -> &StatementKind {
        &self.interned_statement_kinds[id.0]
    }

    pub fn push_lvalue(&mut self, lvalue: LValue) -> InternedExpressionKind {
        self.push_expression_kind(lvalue.as_expression().kind)
    }

    pub fn get_lvalue(&self, id: InternedExpressionKind, location: Location) -> LValue {
        LValue::from_expression_kind(self.get_expression_kind(id).clone(), location)
            .expect("Called LValue::from_expression with an invalid expression")
    }

    pub fn push_pattern(&mut self, pattern: Pattern) -> InternedPattern {
        InternedPattern(self.interned_patterns.insert(pattern))
    }

    pub fn get_pattern(&self, id: InternedPattern) -> &Pattern {
        &self.interned_patterns[id.0]
    }

    pub fn push_unresolved_type_data(
        &mut self,
        typ: UnresolvedTypeData,
    ) -> InternedUnresolvedTypeData {
        InternedUnresolvedTypeData(self.interned_unresolved_type_data.insert(typ))
    }

    pub fn get_unresolved_type_data(&self, id: InternedUnresolvedTypeData) -> &UnresolvedTypeData {
        &self.interned_unresolved_type_data[id.0]
    }

    /// Returns the type of an operator (which is always a function), along with its return type.
    pub fn get_infix_operator_type(
        &self,
        lhs: ExprId,
        operator: BinaryOpKind,
        operator_expr: ExprId,
    ) -> (Type, Type) {
        let lhs_type = self.id_type(lhs);
        let args = vec![lhs_type.clone(), lhs_type];

        // If this is a comparison operator, the result is a boolean but
        // the actual method call returns an Ordering
        use crate::ast::BinaryOpKind::*;
        let ret = if matches!(operator, Less | LessEqual | Greater | GreaterEqual) {
            self.ordering_type()
        } else {
            self.id_type(operator_expr)
        };

        let env = Box::new(Type::Unit);
        (Type::Function(args, Box::new(ret.clone()), env, false), ret)
    }

    /// Returns the type of a prefix operator (which is always a function), along with its return type.
    pub fn get_prefix_operator_type(&self, operator_expr: ExprId, rhs: ExprId) -> (Type, Type) {
        let rhs_type = self.id_type(rhs);
        let args = vec![rhs_type];
        let ret = self.id_type(operator_expr);
        let env = Box::new(Type::Unit);
        (Type::Function(args, Box::new(ret.clone()), env, false), ret)
    }

    pub fn is_in_lsp_mode(&self) -> bool {
        self.lsp_mode
    }

    /// Sets the associated types for the given trait impl.
    /// Each type in [`NamedType`] will be wrapped in a [`Type::TypeVariable`] if it's of kind [`Kind::Numeric`].
    pub(crate) fn set_associated_types_for_impl(
        &mut self,
        impl_id: TraitImplId,
        associated_types: Vec<NamedType>,
    ) {
        // Wrap the named generics in type variables to be able to refer them as type variables
        for associated_type in &associated_types {
            let Kind::Numeric(numeric_type) = associated_type.typ.kind() else {
                continue;
            };

            let name = associated_type.name.to_string();
            let definition_id = self.push_definition(
                associated_type.name.to_string(),
                false,
                false,
                DefinitionKind::AssociatedConstant(impl_id, name.clone()),
                associated_type.name.location(),
            );
            self.trait_impl_associated_constants
                .entry(impl_id)
                .or_default()
                .insert(name, (definition_id, *numeric_type));
        }

        self.trait_impl_associated_types.insert(impl_id, associated_types);
    }

    /// Returns the associated types for the given trait impl.
    /// The Type of each [`NamedType`] that is an associated constant is guaranteed to be a [`Type::TypeVariable`].
    pub fn get_associated_types_for_impl(&self, impl_id: TraitImplId) -> &[NamedType] {
        &self.trait_impl_associated_types[&impl_id]
    }

    pub fn find_associated_type_for_impl(
        &self,
        impl_id: TraitImplId,
        type_name: &str,
    ) -> Option<&Type> {
        let types = self.trait_impl_associated_types.get(&impl_id)?;
        types.iter().find(|typ| typ.name.as_str() == type_name).map(|typ| &typ.typ)
    }

    /// Returns the definition id for the associated constant of the given type variable.
    pub fn get_trait_impl_associated_constant(
        &self,
        impl_id: TraitImplId,
        name: &str,
    ) -> Option<&(DefinitionId, Type)> {
        self.trait_impl_associated_constants.get(&impl_id).and_then(|map| map.get(name))
    }

    /// Return a set of TypeBindings to bind types from the parent trait to those from the trait impl.
    pub fn trait_to_impl_bindings(
        &self,
        trait_id: TraitId,
        impl_id: TraitImplId,
        trait_impl_generics: &[Type],
        impl_self_type: Type,
    ) -> TypeBindings {
        let mut bindings = TypeBindings::default();
        let the_trait = self.get_trait(trait_id);
        let trait_generics = the_trait.generics.clone();

        let self_type_var = the_trait.self_type_typevar.clone();
        bindings.insert(
            self_type_var.id(),
            (self_type_var.clone(), self_type_var.kind(), impl_self_type),
        );

        for (trait_generic, trait_impl_generic) in trait_generics.iter().zip(trait_impl_generics) {
            let type_var = trait_generic.type_var.clone();
            bindings.insert(
                type_var.id(),
                (type_var, trait_generic.kind(), trait_impl_generic.clone()),
            );
        }

        // Now that the normal bindings are added, we still need to bind the associated types
        let impl_associated_types = self.get_associated_types_for_impl(impl_id);
        let trait_associated_types = &the_trait.associated_types;

        // `impl_associated_types` may not be in the same order as `trait_associated_types`
        let impl_associated_types = impl_associated_types
            .iter()
            .map(|typ| (typ.name.as_str(), typ))
            .collect::<HashMap<_, _>>();

        for trait_type in trait_associated_types {
            let Some(impl_type) = impl_associated_types.get(trait_type.name.as_str()) else {
                // Impl doesn't have the corresponding associated type - an error should already
                // have been issued beforehand.
                continue;
            };

            let type_variable = trait_type.type_var.clone();
            bindings.insert(
                type_variable.id(),
                (type_variable, trait_type.kind(), impl_type.typ.clone()),
            );
        }

        bindings
    }

    pub fn set_doc_comments(&mut self, id: ReferenceId, doc_comments: Vec<String>) {
        if !doc_comments.is_empty() {
            self.doc_comments.insert(id, doc_comments);
        }
    }

    pub fn doc_comments(&self, id: ReferenceId) -> Option<&Vec<String>> {
        self.doc_comments.get(&id)
    }

    pub fn get_expr_id_from_index(&self, index: impl Into<Index>) -> Option<ExprId> {
        let index = index.into();
        match self.nodes.get(index) {
            Some(Node::Expression(_)) => Some(ExprId(index)),
            _ => None,
        }
    }

    pub fn add_reexport(
        &mut self,
        module_def_id: ModuleDefId,
        module_id: ModuleId,
        name: Ident,
        visibility: ItemVisibility,
    ) {
        self.reexports.entry(module_def_id).or_default().push(Reexport {
            module_id,
            name,
            visibility,
        });
    }

    pub fn get_reexports(&self, module_def_id: ModuleDefId) -> &[Reexport] {
        self.reexports.get(&module_def_id).map_or(&[], |reexport| reexport)
    }

    pub fn get_trait_reexports(&self, trait_id: TraitId) -> &[Reexport] {
        self.get_reexports(ModuleDefId::TraitId(trait_id))
    }

    pub fn get_meta_attribute_name(&self, meta: &MetaAttribute) -> Option<String> {
        match &meta.name {
            MetaAttributeName::Path(path) => Some(path.last_name().to_string()),
            MetaAttributeName::Resolved(expr_id) => {
                let HirExpression::Ident(ident, _) = self.expression(expr_id) else {
                    return None;
                };
                self.try_definition(ident.id).map(|def| def.name.clone())
            }
        }
    }

    /// Gets the dependency graph from the node interner.
    pub fn dependency_graph(&self) -> &DiGraph<DependencyId, ()> {
        &self.dependency_graph
    }

    /// Gets the trait implementations from the node interner.
    pub fn trait_implementations(&self) -> &HashMap<TraitImplId, Shared<TraitImpl>> {
        &self.trait_implementations
    }
}

impl Methods {
    fn add_method(&mut self, method: FuncId, typ: Type, trait_id: Option<TraitId>) {
        if let Some(trait_id) = trait_id {
            let trait_impl_method = TraitImplMethod { typ, method, trait_id };
            self.trait_impl_methods.push(trait_impl_method);
        } else {
            let impl_method = ImplMethod { typ, method };
            self.direct.push(impl_method);
        }
    }

    /// Iterate through each method, starting with the direct methods
    pub fn iter(&self) -> impl Iterator<Item = (FuncId, &Type, Option<TraitId>)> {
        let trait_impl_methods =
            self.trait_impl_methods.iter().map(|m| (m.method, &m.typ, Some(m.trait_id)));
        let direct = self.direct.iter().map(|method| (method.method, &method.typ, None));
        direct.chain(trait_impl_methods)
    }

    pub fn find_matching_methods<'a>(
        &'a self,
        typ: &'a Type,
        has_self_param: bool,
        interner: &'a NodeInterner,
    ) -> impl Iterator<Item = (FuncId, Option<TraitId>)> + 'a {
        self.iter().filter_map(move |(method, method_type, trait_id)| {
            if Self::method_matches(typ, has_self_param, method, method_type, interner) {
                Some((method, trait_id))
            } else {
                None
            }
        })
    }

    pub fn find_direct_method(
        &self,
        typ: &Type,
        check_self_param: bool,
        interner: &NodeInterner,
    ) -> Option<FuncId> {
        for method in &self.direct {
            if Self::method_matches(typ, check_self_param, method.method, &method.typ, interner) {
                return Some(method.method);
            }
        }

        None
    }

    pub fn find_trait_methods(
        &self,
        typ: &Type,
        has_self_param: bool,
        interner: &NodeInterner,
    ) -> Vec<(FuncId, TraitId)> {
        let mut results = Vec::new();

        for trait_impl_method in &self.trait_impl_methods {
            let method = trait_impl_method.method;
            let method_type = &trait_impl_method.typ;
            let trait_id = trait_impl_method.trait_id;

            if Self::method_matches(typ, has_self_param, method, method_type, interner) {
                results.push((method, trait_id));
            }
        }

        results
    }

    fn method_matches(
        typ: &Type,
        check_self_param: bool,
        method: FuncId,
        method_type: &Type,
        interner: &NodeInterner,
    ) -> bool {
        match interner.function_meta(&method).typ.instantiate(interner).0 {
            Type::Function(args, _, _, _) => {
                if check_self_param {
                    if let Some(object) = args.first() {
                        if object.unify(typ).is_ok() {
                            return true;
                        }

                        // Handle auto-dereferencing `&T` and `&mut T` into `T`
                        if let Type::Reference(object, _mutable) = object {
                            if object.unify(typ).is_ok() {
                                return true;
                            }
                        }
                    }
                } else {
                    // We still need to make sure the method is for the given type
                    // (this might be false if for example a method for `Struct<i32>` was added but
                    // now we are looking for a method in `Struct<i64>`)
                    if method_type.unify(typ).is_ok() {
                        return true;
                    }

                    // Handle auto-dereferencing `&T` and `&mut T` into `T`
                    if let Type::Reference(method_type, _mutable) = method_type {
                        if method_type.unify(typ).is_ok() {
                            return true;
                        }
                    }
                }
            }
            Type::Error => (),
            other => unreachable!("Expected function type, found {other}"),
        }

        false
    }
}

/// These are the primitive type variants that we support adding methods to
#[derive(Copy, Clone, Hash, PartialEq, Eq, Debug)]
enum TypeMethodKey {
    /// Fields and integers share methods for ease of use. These methods may still
    /// accept only fields or integers, it is just that their names may not clash.
    FieldOrInt,
    Array,
    Slice,
    Bool,
    String,
    FmtString,
    Unit,
    Tuple,
    Function,
    Generic,
    Quoted(QuotedType),
    Struct(TypeId),
}

fn get_type_method_key(typ: &Type) -> Option<TypeMethodKey> {
    use TypeMethodKey::*;
    let typ = typ.follow_bindings();
    match &typ {
        Type::FieldElement => Some(FieldOrInt),
        Type::Array(_, _) => Some(Array),
        Type::Slice(_) => Some(Slice),
        Type::Integer(_, _) => Some(FieldOrInt),
        Type::TypeVariable(var) => {
            if var.is_integer() || var.is_integer_or_field() {
                Some(FieldOrInt)
            } else {
                None
            }
        }
        Type::Bool => Some(Bool),
        Type::String(_) => Some(String),
        Type::FmtString(_, _) => Some(FmtString),
        Type::Unit => Some(Unit),
        Type::Tuple(_) => Some(Tuple),
        Type::Function(_, _, _, _) => Some(Function),
        Type::NamedGeneric(_) => Some(Generic),
        Type::Quoted(quoted) => Some(Quoted(*quoted)),
        Type::Reference(element, _) => get_type_method_key(element),
        Type::Alias(alias, _) => get_type_method_key(&alias.borrow().typ),
        Type::DataType(struct_type, _) => Some(Struct(struct_type.borrow().id)),

        // We do not support adding methods to these types
        Type::Forall(_, _)
        | Type::Constant(..)
        | Type::Error
        | Type::InfixExpr(..)
        | Type::CheckedCast { .. }
        | Type::TraitAsType(..) => None,
    }
}
