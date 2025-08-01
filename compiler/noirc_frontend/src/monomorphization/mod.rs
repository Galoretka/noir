//! Coming after type checking, monomorphization is the last pass in Noir's frontend.
//! It accepts the type checked HIR as input and produces a monomorphized AST as output.
//! This file implements the pass itself, while the AST is defined in the ast module.
//!
//! Unlike the HIR, which is stored within the NodeInterner, the monomorphized AST is
//! self-contained and does not need an external context struct. As a result, the NodeInterner
//! can be safely discarded after monomorphization.
//!
//! The entry point to this pass is the `monomorphize` function which, starting from a given
//! function, will monomorphize the entire reachable program.
use crate::NamedGeneric;
use crate::ast::{FunctionKind, IntegerBitSize, ItemVisibility, UnaryOp};
use crate::hir::comptime::InterpreterError;
use crate::hir::type_check::{NoMatchingImplFoundError, TypeCheckError};
use crate::node_interner::{ExprId, GlobalValue, ImplSearchErrorKind, TraitItemId};
use crate::shared::{Signedness, Visibility};
use crate::signed_field::SignedField;
use crate::token::FmtStrFragment;
use crate::{
    Kind, Type, TypeBinding, TypeBindings,
    debug::DebugInstrumenter,
    hir_def::{
        expr::*,
        function::{FunctionSignature, Parameters},
        stmt::{HirAssignStatement, HirLValue, HirLetStatement, HirPattern, HirStatement},
        types,
    },
    node_interner::{self, DefinitionKind, NodeInterner, StmtId, TraitImplKind},
};
use acvm::{FieldElement, acir::AcirField};
use ast::{GlobalId, IdentId, While};
use fxhash::{FxHashMap as HashMap, FxHashSet as HashSet};
use iter_extended::{btree_map, try_vecmap, vecmap};
use noirc_errors::Location;
use noirc_printable_type::PrintableType;
use std::{
    collections::{BTreeMap, VecDeque},
    unreachable,
};

use self::ast::InlineType;
use self::debug_types::DebugTypeTracker;
use self::{
    ast::{Definition, FuncId, Function, LocalId, Program},
    errors::MonomorphizationError,
};

pub mod ast;
mod debug;
pub mod debug_types;
pub mod errors;
pub mod printer;
pub mod tests;

struct LambdaContext {
    env_ident: ast::Ident,
    captures: Vec<HirCapturedVar>,
}

/// The context struct for the monomorphization pass.
///
/// This struct holds the FIFO queue of functions to monomorphize, which is added to
/// whenever a new (function, type) combination is encountered.
pub(super) struct Monomorphizer<'interner> {
    /// Functions are keyed by their unique ID, whether they're unconstrained, their expected type,
    /// and any generics they have so that we can monomorphize a new version of the function for each type.
    ///
    /// Keying by any turbofish generics that are specified is necessary for a case where we may have a
    /// trait generic that can be instantiated outside of a function parameter or return value.
    functions: Functions,

    /// Unlike functions, locals are only keyed by their unique ID because they are never
    /// duplicated during monomorphization. Doing so would allow them to be used polymorphically
    /// but would also cause them to be re-evaluated which is a performance trap that would
    /// confuse users.
    locals: HashMap<node_interner::DefinitionId, LocalId>,

    /// Globals are keyed by their unique ID because they are never duplicated during monomorphization.
    globals: HashMap<node_interner::GlobalId, GlobalId>,

    finished_globals: HashMap<GlobalId, (String, ast::Type, ast::Expression)>,

    /// Queue of functions to monomorphize next each item in the queue is a tuple of:
    /// (old_id, new_monomorphized_id, any type bindings to apply, the trait method if old_id is from a trait impl, is_unconstrained, location)
    queue: VecDeque<(
        node_interner::FuncId,
        FuncId,
        TypeBindings,
        Option<TraitItemId>,
        bool,
        Location,
    )>,

    /// When a function finishes being monomorphized, the monomorphized ast::Function is
    /// stored here along with its FuncId.
    finished_functions: BTreeMap<FuncId, Function>,

    /// Used to reference existing definitions in the HIR
    interner: &'interner mut NodeInterner,

    lambda_envs_stack: Vec<LambdaContext>,

    next_local_id: u32,
    next_global_id: u32,
    next_function_id: u32,
    next_ident_id: u32,

    is_range_loop: bool,

    return_location: Option<Location>,

    debug_type_tracker: DebugTypeTracker,

    in_unconstrained_function: bool,
}

/// Using nested HashMaps here lets us avoid cloning HirTypes when calling .get()
type Functions = HashMap<
    (node_interner::FuncId, /*is_unconstrained:*/ bool),
    HashMap<HirType, HashMap<Vec<HirType>, FuncId>>,
>;

type HirType = crate::Type;

/// Starting from the given `main` function, monomorphize the entire program,
/// replacing all references to type variables and NamedGenerics with concrete
/// types, duplicating definitions as necessary to do so.
///
/// Instead of iterating over every function, this pass starts with the main function
/// and monomorphizes every function reachable from it via function calls and references.
/// Thus, if a function is not used in the program, it will not be monomorphized.
///
/// Note that there is no requirement on the `main` function that can be passed into
/// this function. Typically, this is the function named "main" in the source project,
/// but it can also be, for example, an arbitrary test function for running `nargo test`.
#[tracing::instrument(level = "trace", skip(main, interner))]
pub fn monomorphize(
    main: node_interner::FuncId,
    interner: &mut NodeInterner,
    force_unconstrained: bool,
) -> Result<Program, MonomorphizationError> {
    monomorphize_debug(main, interner, &DebugInstrumenter::default(), force_unconstrained)
}

pub fn monomorphize_debug(
    main: node_interner::FuncId,
    interner: &mut NodeInterner,
    debug_instrumenter: &DebugInstrumenter,
    force_unconstrained: bool,
) -> Result<Program, MonomorphizationError> {
    let debug_type_tracker = DebugTypeTracker::build_from_debug_instrumenter(debug_instrumenter);
    let mut monomorphizer = Monomorphizer::new(interner, debug_type_tracker);
    monomorphizer.in_unconstrained_function = force_unconstrained;
    let function_sig = monomorphizer.compile_main(main)?;

    while !monomorphizer.queue.is_empty() {
        let (next_fn_id, new_id, bindings, trait_method, is_unconstrained, location) =
            monomorphizer.queue.pop_front().unwrap();
        monomorphizer.locals.clear();

        monomorphizer.in_unconstrained_function = is_unconstrained;

        perform_instantiation_bindings(&bindings);
        let interner = &monomorphizer.interner;
        let impl_bindings = perform_impl_bindings(interner, trait_method, next_fn_id, location)
            .map_err(MonomorphizationError::InterpreterError)?;

        monomorphizer.function(next_fn_id, new_id, location)?;
        undo_instantiation_bindings(impl_bindings);
        undo_instantiation_bindings(bindings);
    }

    let func_sigs = monomorphizer
        .finished_functions
        .iter()
        .flat_map(|(_, f)| {
            if (!force_unconstrained && f.inline_type.is_entry_point())
                || f.id == Program::main_id()
            {
                Some(f.func_sig.clone())
            } else {
                None
            }
        })
        .collect();

    let functions = vecmap(monomorphizer.finished_functions, |(_, f)| f);

    let globals = monomorphizer.finished_globals.into_iter().collect::<BTreeMap<_, _>>();

    let (debug_variables, debug_functions, debug_types) =
        monomorphizer.debug_type_tracker.extract_vars_and_types();

    let program = Program::new(
        functions,
        func_sigs,
        function_sig,
        monomorphizer.return_location,
        globals,
        debug_variables,
        debug_functions,
        debug_types,
    );
    Ok(program.handle_ownership())
}

impl<'interner> Monomorphizer<'interner> {
    pub(crate) fn new(
        interner: &'interner mut NodeInterner,
        debug_type_tracker: DebugTypeTracker,
    ) -> Self {
        Monomorphizer {
            functions: HashMap::default(),
            locals: HashMap::default(),
            globals: HashMap::default(),
            finished_globals: HashMap::default(),
            queue: VecDeque::new(),
            finished_functions: BTreeMap::new(),
            next_local_id: 0,
            next_global_id: 0,
            next_function_id: 0,
            next_ident_id: 0,
            interner,
            lambda_envs_stack: Vec::new(),
            is_range_loop: false,
            return_location: None,
            debug_type_tracker,
            in_unconstrained_function: false,
        }
    }

    pub(super) fn next_local_id(&mut self) -> LocalId {
        let id = self.next_local_id;
        self.next_local_id += 1;
        LocalId(id)
    }

    fn next_function_id(&mut self) -> ast::FuncId {
        let id = self.next_function_id;
        self.next_function_id += 1;
        ast::FuncId(id)
    }

    fn next_global_id(&mut self) -> GlobalId {
        let id = self.next_global_id;
        self.next_global_id += 1;
        GlobalId(id)
    }

    pub(super) fn next_ident_id(&mut self) -> IdentId {
        let id = self.next_ident_id;
        self.next_ident_id += 1;
        IdentId(id)
    }

    fn lookup_local(&mut self, id: node_interner::DefinitionId) -> Option<Definition> {
        self.locals.get(&id).copied().map(Definition::Local)
    }

    fn lookup_function(
        &mut self,
        id: node_interner::FuncId,
        expr_id: node_interner::ExprId,
        typ: &HirType,
        turbofish_generics: &[HirType],
        trait_method: Option<TraitItemId>,
    ) -> Definition {
        let typ = typ.follow_bindings();
        let turbofish_generics = vecmap(turbofish_generics, |typ| typ.follow_bindings());
        let is_unconstrained = self.is_unconstrained(id);

        match self
            .functions
            .get(&(id, is_unconstrained))
            .and_then(|inner_map| inner_map.get(&typ))
            .and_then(|inner_map| inner_map.get(&turbofish_generics))
        {
            Some(id) => Definition::Function(*id),
            None => {
                // Function has not been monomorphized yet
                let attributes = self.interner.function_attributes(&id);
                match self.interner.function_meta(&id).kind {
                    FunctionKind::LowLevel => {
                        let attribute = attributes.function().expect("all low level functions must contain a function attribute which contains the opcode which it links to");
                        let opcode = attribute.kind.foreign().expect(
                            "ice: function marked as foreign, but attribute kind does not match this",
                        );
                        Definition::LowLevel(opcode.to_string())
                    }
                    FunctionKind::Builtin => {
                        let attribute = attributes.function().expect("all builtin functions must contain a function attribute which contains the opcode which it links to");
                        let opcode = attribute.kind.builtin().expect(
                            "ice: function marked as builtin, but attribute kind does not match this",
                        );
                        Definition::Builtin(opcode.to_string())
                    }
                    FunctionKind::Normal | FunctionKind::TraitFunctionWithoutBody => {
                        let id =
                            self.queue_function(id, expr_id, typ, turbofish_generics, trait_method);
                        Definition::Function(id)
                    }
                    FunctionKind::Oracle => {
                        let attribute = attributes.function().expect("all oracle functions must contain a function attribute which contains the opcode which it links to");
                        let opcode = attribute.kind.oracle().expect(
                            "ice: function marked as builtin, but attribute kind does not match this",
                        );
                        Definition::Oracle(opcode.to_string())
                    }
                }
            }
        }
    }

    fn define_local(&mut self, id: node_interner::DefinitionId, new_id: LocalId) {
        self.locals.insert(id, new_id);
    }

    /// Prerequisite: typ = typ.follow_bindings()
    ///          and: turbofish_generics = vecmap(turbofish_generics, Type::follow_bindings)
    fn define_function(
        &mut self,
        id: node_interner::FuncId,
        typ: HirType,
        turbofish_generics: Vec<HirType>,
        is_unconstrained: bool,
        new_id: FuncId,
    ) {
        self.functions
            .entry((id, is_unconstrained))
            .or_default()
            .entry(typ)
            .or_default()
            .insert(turbofish_generics, new_id);
    }

    fn compile_main(
        &mut self,
        main_id: node_interner::FuncId,
    ) -> Result<FunctionSignature, MonomorphizationError> {
        let new_main_id = self.next_function_id();
        assert_eq!(new_main_id, Program::main_id());

        let location = self.interner.function_meta(&main_id).location;
        self.in_unconstrained_function = self.is_unconstrained(main_id);
        self.function(main_id, new_main_id, location)?;

        self.return_location =
            self.interner.function(&main_id).block(self.interner).statements().last().and_then(
                |x| match self.interner.statement(x) {
                    HirStatement::Expression(id) => Some(self.interner.id_location(id)),
                    _ => None,
                },
            );
        let main_meta = self.interner.function_meta(&main_id);
        Ok(main_meta.function_signature())
    }

    fn function(
        &mut self,
        f: node_interner::FuncId,
        id: FuncId,
        location: Location,
    ) -> Result<(), MonomorphizationError> {
        if let Some((self_type, trait_id)) = self.interner.get_function_trait(&f) {
            let the_trait = self.interner.get_trait(trait_id);
            the_trait.self_type_typevar.force_bind(self_type);
        }

        let meta = self.interner.function_meta(&f).clone();

        let mut func_sig = meta.function_signature();
        // Follow the bindings of the function signature for entry points
        // which are not `main` such as foldable functions.
        for param in func_sig.0.iter_mut() {
            param.1 = param.1.follow_bindings();
        }
        func_sig.1 = func_sig.1.map(|return_type| return_type.follow_bindings());

        let modifiers = self.interner.function_modifiers(&f);
        let name = self.interner.function_name(&f).to_owned();

        if modifiers.is_comptime {
            return Err(MonomorphizationError::ComptimeFnInRuntimeCode { name, location });
        }

        let body_expr_id = self.interner.function(&f).as_expr();
        let body_return_type = self.interner.id_type(body_expr_id);
        let return_type = match meta.return_type() {
            Type::TraitAsType(..) => &body_return_type,
            other => other,
        };

        // If `convert_type` fails here it is most likely because of generics at the
        // call site after instantiating this function's type. So show the error there
        // instead of at the function definition.
        let return_type = Self::convert_type(return_type, location)?;
        let return_visibility = meta.return_visibility;
        let unconstrained = self.in_unconstrained_function;

        let attributes = self.interner.function_attributes(&f);
        let inline_type = InlineType::from(attributes);

        let parameters = self.parameters(&meta.parameters)?;
        let body = self.expr(body_expr_id)?;
        let function = ast::Function {
            id,
            name,
            parameters,
            body,
            return_type,
            return_visibility,
            unconstrained,
            inline_type,
            func_sig,
        };

        self.push_function(id, function);
        Ok(())
    }

    fn push_function(&mut self, id: FuncId, function: ast::Function) {
        let existing = self.finished_functions.insert(id, function);
        assert!(existing.is_none());
    }

    /// Monomorphize each parameter, expanding tuple/struct patterns into multiple parameters
    /// and binding any generic types found.
    #[allow(clippy::type_complexity)]
    fn parameters(
        &mut self,
        params: &Parameters,
    ) -> Result<Vec<(ast::LocalId, bool, String, ast::Type, Visibility)>, MonomorphizationError>
    {
        let mut new_params = Vec::with_capacity(params.len());
        for (parameter, typ, visibility) in &params.0 {
            self.parameter(parameter, typ, visibility, &mut new_params)?;
        }
        Ok(new_params)
    }

    fn parameter(
        &mut self,
        param: &HirPattern,
        typ: &HirType,
        visibility: &Visibility,
        new_params: &mut Vec<(ast::LocalId, bool, String, ast::Type, Visibility)>,
    ) -> Result<(), MonomorphizationError> {
        match param {
            HirPattern::Identifier(ident) => {
                let new_id = self.next_local_id();
                let definition = self.interner.definition(ident.id);
                let name = definition.name.clone();
                let typ = Self::convert_type(typ, ident.location)?;
                new_params.push((new_id, definition.mutable, name, typ, *visibility));
                self.define_local(ident.id, new_id);
            }
            HirPattern::Mutable(pattern, _) => {
                self.parameter(pattern, typ, visibility, new_params)?;
            }
            HirPattern::Tuple(fields, _) => {
                let tuple_field_types = unwrap_tuple_type(typ);

                for (field, typ) in fields.iter().zip(tuple_field_types) {
                    self.parameter(field, &typ, visibility, new_params)?;
                }
            }
            HirPattern::Struct(_, fields, location) => {
                let struct_field_types = unwrap_struct_type(typ, *location)?;
                assert_eq!(struct_field_types.len(), fields.len());

                let mut fields = btree_map(fields, |(name, field)| (name.to_string(), field));

                // Iterate over `struct_field_types` since `unwrap_struct_type` will always
                // return the fields in the order defined by the struct type.
                for (field_name, field_type, _) in struct_field_types {
                    let field = fields.remove(&field_name).unwrap_or_else(|| {
                        unreachable!("Expected a field named '{field_name}' in the struct pattern")
                    });

                    self.parameter(field, &field_type, visibility, new_params)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn expr(
        &mut self,
        expr: node_interner::ExprId,
    ) -> Result<ast::Expression, MonomorphizationError> {
        use ast::Expression::Literal;
        use ast::Literal::*;

        let expr = match self.interner.expression(&expr) {
            HirExpression::Ident(ident, generics) => self.ident(ident, expr, generics)?,
            HirExpression::Literal(HirLiteral::Str(contents)) => Literal(Str(contents)),
            HirExpression::Literal(HirLiteral::FmtStr(fragments, idents, _length)) => {
                let fields = try_vecmap(idents, |ident| self.expr(ident))?;
                Literal(FmtStr(
                    fragments,
                    fields.len() as u64,
                    Box::new(ast::Expression::Tuple(fields)),
                ))
            }
            HirExpression::Literal(HirLiteral::Bool(value)) => Literal(Bool(value)),
            HirExpression::Literal(HirLiteral::Integer(value)) => {
                let location = self.interner.id_location(expr);
                let typ = Self::convert_type(&self.interner.id_type(expr), location)?;
                Literal(Integer(value, typ, location))
            }
            HirExpression::Literal(HirLiteral::Array(array)) => match array {
                HirArrayLiteral::Standard(array) => self.standard_array(expr, array, false)?,
                HirArrayLiteral::Repeated { repeated_element, length } => {
                    self.repeated_array(expr, repeated_element, length, false)?
                }
            },
            HirExpression::Literal(HirLiteral::Slice(array)) => match array {
                HirArrayLiteral::Standard(array) => self.standard_array(expr, array, true)?,
                HirArrayLiteral::Repeated { repeated_element, length } => {
                    self.repeated_array(expr, repeated_element, length, true)?
                }
            },
            HirExpression::Literal(HirLiteral::Unit) => ast::Expression::Block(vec![]),
            HirExpression::Block(block) => self.block(block.statements)?,
            HirExpression::Unsafe(block) => self.block(block.statements)?,

            HirExpression::Prefix(prefix) => {
                let rhs = self.expr(prefix.rhs)?;
                let location = self.interner.expr_location(&expr);

                if self.interner.get_selected_impl_for_expression(expr).is_some() {
                    // If an impl was selected for this prefix operator, replace it
                    // with a method call to the appropriate trait impl method.
                    let (function_type, ret) =
                        self.interner.get_prefix_operator_type(expr, prefix.rhs);

                    let method = prefix
                        .trait_method_id
                        .expect("ice: missing trait method if when impl was found");
                    let func = self.resolve_trait_item_expr(expr, function_type, method)?;
                    self.create_prefix_operator_impl_call(func, rhs, ret, location)?
                } else {
                    let operator = prefix.operator;
                    let rhs = Box::new(rhs);
                    let result_type = Self::convert_type(&self.interner.id_type(expr), location)?;
                    ast::Expression::Unary(ast::Unary {
                        operator,
                        rhs,
                        result_type,
                        location,
                        skip: prefix.skip,
                    })
                }
            }

            HirExpression::Infix(infix) => {
                let lhs = self.expr(infix.lhs)?;
                let rhs = self.expr(infix.rhs)?;
                let operator = infix.operator.kind;
                let location = self.interner.expr_location(&expr);
                if self.interner.get_selected_impl_for_expression(expr).is_some() {
                    // If an impl was selected for this infix operator, replace it
                    // with a method call to the appropriate trait impl method.
                    let (function_type, ret) =
                        self.interner.get_infix_operator_type(infix.lhs, operator, expr);

                    let method = infix.trait_method_id;
                    let func = self.resolve_trait_item_expr(expr, function_type, method)?;
                    let operator = infix.operator;
                    self.create_infix_operator_impl_call(func, lhs, operator, rhs, ret, location)?
                } else {
                    let lhs = Box::new(lhs);
                    let rhs = Box::new(rhs);
                    ast::Expression::Binary(ast::Binary { lhs, rhs, operator, location })
                }
            }

            HirExpression::Index(index) => self.index(expr, index)?,

            HirExpression::MemberAccess(access) => {
                let field_index = self.interner.get_field_index(expr);
                let expr = Box::new(self.expr(access.lhs)?);
                ast::Expression::ExtractTupleField(expr, field_index)
            }

            HirExpression::Call(call) => self.function_call(call, expr)?,

            HirExpression::Constrain(constrain) => {
                let expr = self.expr(constrain.0)?;
                let location = self.interner.expr_location(&constrain.0);
                let assert_message = constrain
                    .2
                    .map(|assert_msg_expr| {
                        self.expr(assert_msg_expr).map(|expr| {
                            (expr, self.interner.id_type(assert_msg_expr).follow_bindings())
                        })
                    })
                    .transpose()?
                    .map(Box::new);

                ast::Expression::Constrain(Box::new(expr), location, assert_message)
            }

            HirExpression::Cast(cast) => {
                let location = self.interner.expr_location(&expr);
                let typ = Self::convert_type(&cast.r#type, location)?;
                let lhs = Box::new(self.expr(cast.lhs)?);
                ast::Expression::Cast(ast::Cast { lhs, r#type: typ, location })
            }

            HirExpression::If(if_expr) => {
                let condition = Box::new(self.expr(if_expr.condition)?);
                let consequence = Box::new(self.expr(if_expr.consequence)?);
                let else_ =
                    if_expr.alternative.map(|alt| self.expr(alt)).transpose()?.map(Box::new);

                let location = self.interner.expr_location(&expr);
                let frontend_type = self.interner.id_type(expr);
                let typ = Self::convert_type(&frontend_type, location)?;

                if !self.in_unconstrained_function && Self::contains_reference(&frontend_type) {
                    let typ = frontend_type.to_string();
                    return Err(MonomorphizationError::ReferenceReturnedFromIfOrMatch {
                        typ,
                        location,
                    });
                }

                ast::Expression::If(ast::If { condition, consequence, alternative: else_, typ })
            }

            HirExpression::Match(match_expr) => self.match_expr(match_expr, expr)?,

            HirExpression::Tuple(fields) => {
                let fields = try_vecmap(fields, |id| self.expr(id))?;
                ast::Expression::Tuple(fields)
            }
            HirExpression::Constructor(constructor) => self.constructor(constructor, expr)?,

            HirExpression::Lambda(lambda) => self.lambda(lambda, expr)?,

            HirExpression::Error => unreachable!("Encountered Error node during monomorphization"),
            HirExpression::Quote(_) => unreachable!("quote expression remaining in runtime code"),
            HirExpression::Unquote(_) => {
                unreachable!("unquote expression remaining in runtime code")
            }
            HirExpression::EnumConstructor(constructor) => {
                self.enum_constructor(constructor, expr)?
            }
        };

        Ok(expr)
    }

    fn contains_reference(typ: &types::Type) -> bool {
        match typ {
            Type::FieldElement
            | Type::Bool
            | Type::String(_)
            | Type::Integer(..)
            | Type::Unit
            | Type::TraitAsType(..)
            | Type::Constant(..)
            | Type::Quoted(..)
            | Type::InfixExpr(..)
            | Type::Error => false,

            Type::Reference(_, _) => true,

            Type::Array(_len, element) => Self::contains_reference(element),
            Type::Slice(element) => Self::contains_reference(element),
            Type::FmtString(_, environment) => Self::contains_reference(environment),
            Type::Tuple(fields) => fields.iter().any(Self::contains_reference),
            Type::DataType(datatype, generics) => {
                let datatype = datatype.borrow();
                if let Some(fields) = datatype.get_fields(generics) {
                    fields.iter().any(|(_, field, _)| Self::contains_reference(field))
                } else if let Some(variants) = datatype.get_variants(generics) {
                    variants
                        .iter()
                        .any(|(_, variant_args)| variant_args.iter().any(Self::contains_reference))
                } else {
                    false
                }
            }
            Type::Alias(alias, generics) => {
                Self::contains_reference(&alias.borrow().get_type(generics))
            }
            Type::TypeVariable(type_variable) => match &*type_variable.borrow() {
                TypeBinding::Bound(binding) => Self::contains_reference(binding),
                TypeBinding::Unbound(..) => false,
            },
            Type::NamedGeneric(named_generic) => match &*named_generic.type_var.borrow() {
                TypeBinding::Bound(binding) => Self::contains_reference(binding),
                TypeBinding::Unbound(..) => false,
            },
            Type::CheckedCast { to, .. } => Self::contains_reference(to),
            Type::Function(_args, _ret, env, _unconstrained) => {
                // Only the environment of a function is counted as an actual reference value.
                // Otherwise we can't return functions accepting references as arguments from if
                // expressions.
                Self::contains_reference(env)
            }
            Type::Forall(_, typ) => Self::contains_reference(typ),
        }
    }

    fn standard_array(
        &mut self,
        array: node_interner::ExprId,
        array_elements: Vec<node_interner::ExprId>,
        is_slice: bool,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let location = self.interner.expr_location(&array);
        let typ = Self::convert_type(&self.interner.id_type(array), location)?;
        let contents = try_vecmap(array_elements, |id| self.expr(id))?;
        if is_slice {
            Ok(ast::Expression::Literal(ast::Literal::Slice(ast::ArrayLiteral { contents, typ })))
        } else {
            Ok(ast::Expression::Literal(ast::Literal::Array(ast::ArrayLiteral { contents, typ })))
        }
    }

    fn repeated_array(
        &mut self,
        array: node_interner::ExprId,
        repeated_element: node_interner::ExprId,
        length: HirType,
        is_slice: bool,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let location = self.interner.expr_location(&array);
        let typ = Self::convert_type(&self.interner.id_type(array), location)?;

        let length = length.evaluate_to_u32(location).map_err(|err| {
            let location = self.interner.expr_location(&array);
            MonomorphizationError::UnknownArrayLength { location, err, length }
        })?;

        let contents = try_vecmap(0..length, |_| self.expr(repeated_element))?;
        if is_slice {
            Ok(ast::Expression::Literal(ast::Literal::Slice(ast::ArrayLiteral { contents, typ })))
        } else {
            Ok(ast::Expression::Literal(ast::Literal::Array(ast::ArrayLiteral { contents, typ })))
        }
    }

    fn index(
        &mut self,
        id: node_interner::ExprId,
        index: HirIndexExpression,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let location = self.interner.expr_location(&id);
        let element_type = Self::convert_type(&self.interner.id_type(id), location)?;

        let collection = Box::new(self.expr(index.collection)?);
        let index = Box::new(self.expr(index.index)?);
        let location = self.interner.expr_location(&id);
        Ok(ast::Expression::Index(ast::Index { collection, index, element_type, location }))
    }

    fn statement(&mut self, id: StmtId) -> Result<ast::Expression, MonomorphizationError> {
        match self.interner.statement(&id) {
            HirStatement::Let(let_statement) => self.let_statement(let_statement),
            HirStatement::Assign(assign) => self.assign(assign),
            HirStatement::For(for_loop) => {
                self.is_range_loop = true;
                let start = self.expr(for_loop.start_range)?;
                let end = self.expr(for_loop.end_range)?;
                self.is_range_loop = false;
                let index_variable = self.next_local_id();
                self.define_local(for_loop.identifier.id, index_variable);

                let block = Box::new(self.expr(for_loop.block)?);
                let index_location = for_loop.identifier.location;
                let index_type = self.interner.id_type(for_loop.start_range);
                let index_type = Self::convert_type(&index_type, index_location)?;

                Ok(ast::Expression::For(ast::For {
                    index_variable,
                    index_name: self.interner.definition_name(for_loop.identifier.id).to_owned(),
                    index_type,
                    start_range: Box::new(start),
                    end_range: Box::new(end),
                    start_range_location: self.interner.expr_location(&for_loop.start_range),
                    end_range_location: self.interner.expr_location(&for_loop.end_range),
                    block,
                }))
            }
            HirStatement::Loop(block) => {
                let block = Box::new(self.expr(block)?);
                Ok(ast::Expression::Loop(block))
            }
            HirStatement::While(condition, body) => {
                let condition = Box::new(self.expr(condition)?);
                let body = Box::new(self.expr(body)?);
                Ok(ast::Expression::While(While { condition, body }))
            }
            HirStatement::Expression(expr) => self.expr(expr),
            HirStatement::Semi(expr) => {
                self.expr(expr).map(|expr| ast::Expression::Semi(Box::new(expr)))
            }
            HirStatement::Break => Ok(ast::Expression::Break),
            HirStatement::Continue => Ok(ast::Expression::Continue),
            HirStatement::Error => unreachable!(),

            // All `comptime` statements & expressions should be removed before runtime.
            HirStatement::Comptime(_) => unreachable!("comptime statement in runtime code"),
        }
    }

    fn let_statement(
        &mut self,
        let_statement: HirLetStatement,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let expr = self.expr(let_statement.expression)?;
        let expected_type = self.interner.id_type(let_statement.expression);
        self.unpack_pattern(let_statement.pattern, expr, &expected_type)
    }

    fn constructor(
        &mut self,
        constructor: HirConstructorExpression,
        id: node_interner::ExprId,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let location = self.interner.expr_location(&id);

        let typ = self.interner.id_type(id);
        let field_types = unwrap_struct_type(&typ, location)?;

        let field_type_map =
            btree_map(&field_types, |(name, typ, _)| (name.to_string(), typ.clone()));

        // Create let bindings for each field value first to preserve evaluation order before
        // they are reordered and packed into the resulting tuple
        let mut field_vars = BTreeMap::new();
        let mut new_exprs = Vec::with_capacity(constructor.fields.len());

        for (field_name, expr_id) in constructor.fields {
            let new_id = self.next_local_id();
            let field_type = field_type_map.get(field_name.as_str()).unwrap();
            let location = self.interner.expr_location(&expr_id);
            let typ = Self::convert_type(field_type, location)?;

            field_vars.insert(field_name.to_string(), (new_id, typ));
            let expression = Box::new(self.expr(expr_id)?);

            new_exprs.push(ast::Expression::Let(ast::Let {
                id: new_id,
                mutable: false,
                name: field_name.into_string(),
                expression,
            }));
        }

        // We must ensure the tuple created from the variables here matches the order
        // of the fields as defined in the type. To do this, we iterate over field_types,
        // rather than field_type_map which is a sorted BTreeMap.
        let field_idents = vecmap(field_types, |(name, _, _)| {
            let (id, typ) = field_vars.remove(&name).unwrap_or_else(|| {
                unreachable!("Expected field {name} to be present in constructor for {typ}")
            });

            let definition = Definition::Local(id);
            let mutable = false;
            ast::Expression::Ident(ast::Ident {
                definition,
                mutable,
                location: None,
                name,
                typ,
                id: self.next_ident_id(),
            })
        });

        // Finally we can return the created Tuple from the new block
        new_exprs.push(ast::Expression::Tuple(field_idents));
        Ok(ast::Expression::Block(new_exprs))
    }

    /// For an enum like:
    /// enum Foo {
    ///    A(i32, u32),
    ///    B(Field),
    ///    C
    /// }
    ///
    /// this will translate the call `Foo::A(1, 2)` into `(0, (1, 2), (0,), ())` where
    /// the first field `0` is the tag value, the second is `A`, third is `B`, and fourth is `C`.
    /// Each variant that isn't the desired variant has zeroed values filled in for its data.
    fn enum_constructor(
        &mut self,
        constructor: HirEnumConstructorExpression,
        id: node_interner::ExprId,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let location = self.interner.expr_location(&id);
        let typ = self.interner.id_type(id);
        let variants = unwrap_enum_type(&typ, location)?;

        // Fill in each field of the translated enum tuple.
        // For most fields this will be simply `std::mem::zeroed::<T>()`,
        // but for the given variant we just pack all the arguments into a tuple for that field.
        let mut fields = try_vecmap(variants.into_iter().enumerate(), |(i, (_, arg_types))| {
            let fields = if i == constructor.variant_index {
                try_vecmap(&constructor.arguments, |arg| self.expr(*arg))
            } else {
                try_vecmap(arg_types, |typ| {
                    let typ = Self::convert_type(&typ, location)?;
                    Ok(self.zeroed_value_of_type(&typ, location))
                })
            }?;
            Ok(ast::Expression::Tuple(fields))
        })?;

        let tag_value = FieldElement::from(constructor.variant_index);
        let tag_value = SignedField::positive(tag_value);
        let tag = ast::Literal::Integer(tag_value, ast::Type::Field, location);
        fields.insert(0, ast::Expression::Literal(tag));

        Ok(ast::Expression::Tuple(fields))
    }

    fn block(
        &mut self,
        statement_ids: Vec<StmtId>,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let stmts = try_vecmap(statement_ids, |id| self.statement(id));
        stmts.map(ast::Expression::Block)
    }

    fn unpack_pattern(
        &mut self,
        pattern: HirPattern,
        value: ast::Expression,
        typ: &HirType,
    ) -> Result<ast::Expression, MonomorphizationError> {
        match pattern {
            HirPattern::Identifier(ident) => {
                let new_id = self.next_local_id();
                self.define_local(ident.id, new_id);
                let definition = self.interner.definition(ident.id);

                Ok(ast::Expression::Let(ast::Let {
                    id: new_id,
                    mutable: definition.mutable,
                    name: definition.name.clone(),
                    expression: Box::new(value),
                }))
            }
            HirPattern::Mutable(pattern, _) => self.unpack_pattern(*pattern, value, typ),
            HirPattern::Tuple(patterns, _) => {
                let fields = unwrap_tuple_type(typ);
                self.unpack_tuple_pattern(value, patterns.into_iter().zip(fields), typ)
            }
            HirPattern::Struct(_, patterns, location) => {
                let fields = unwrap_struct_type(typ, location)?;
                assert_eq!(patterns.len(), fields.len());

                let mut patterns =
                    btree_map(patterns, |(name, pattern)| (name.into_string(), pattern));

                // We iterate through the type's fields to match the order defined in the struct type
                let patterns_iter = fields.into_iter().map(|(field_name, field_type, _)| {
                    let pattern = patterns.remove(&field_name).unwrap();
                    (pattern, field_type)
                });

                self.unpack_tuple_pattern(value, patterns_iter, typ)
            }
        }
    }

    fn unpack_tuple_pattern(
        &mut self,
        value: ast::Expression,
        fields: impl Iterator<Item = (HirPattern, HirType)>,
        tuple_type: &Type,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let fresh_id = self.next_local_id();

        let mut definitions = vec![ast::Expression::Let(ast::Let {
            id: fresh_id,
            mutable: false,
            name: "_".into(),
            expression: Box::new(value),
        })];

        for (i, (field_pattern, field_type)) in fields.into_iter().enumerate() {
            let location = field_pattern.location();
            let mutable = false;
            let definition = Definition::Local(fresh_id);
            let name = i.to_string();

            let typ = Self::convert_type(tuple_type, location)?;
            let location = Some(location);
            let id = self.next_ident_id();
            let new_rhs =
                ast::Expression::Ident(ast::Ident { location, mutable, definition, name, typ, id });

            let new_rhs = ast::Expression::ExtractTupleField(Box::new(new_rhs), i);
            let new_expr = self.unpack_pattern(field_pattern, new_rhs, &field_type)?;
            definitions.push(new_expr);
        }

        Ok(ast::Expression::Block(definitions))
    }

    /// Find a captured variable in the innermost closure, and construct an expression
    fn lookup_captured_expr(&mut self, id: node_interner::DefinitionId) -> Option<ast::Expression> {
        let ctx = self.lambda_envs_stack.last()?;
        ctx.captures.iter().position(|capture| capture.ident.id == id).map(|index| {
            ast::Expression::ExtractTupleField(
                Box::new(ast::Expression::Ident(ctx.env_ident.clone())),
                index,
            )
        })
    }

    /// Find a captured variable in the innermost closure construct a LValue
    fn lookup_captured_lvalue(&mut self, id: node_interner::DefinitionId) -> Option<ast::LValue> {
        let ctx = self.lambda_envs_stack.last()?;
        ctx.captures.iter().position(|capture| capture.ident.id == id).map(|index| {
            ast::LValue::MemberAccess {
                object: Box::new(ast::LValue::Ident(ctx.env_ident.clone())),
                field_index: index,
            }
        })
    }

    /// A local (ie non-function) ident only
    fn local_ident(
        &mut self,
        ident: &HirIdent,
        typ: &Type,
    ) -> Result<Option<ast::Ident>, MonomorphizationError> {
        let definition = self.interner.definition(ident.id);
        let name = definition.name.clone();
        let mutable = definition.mutable;

        let Some(definition) = self.lookup_local(ident.id) else {
            return Ok(None);
        };

        let typ = Self::convert_type(typ, ident.location)?;
        let id = self.next_ident_id();
        Ok(Some(ast::Ident { location: Some(ident.location), mutable, definition, name, typ, id }))
    }

    fn ident(
        &mut self,
        ident: HirIdent,
        expr_id: node_interner::ExprId,
        generics: Option<Vec<HirType>>,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let typ = self.interner.id_type(expr_id);

        if let ImplKind::TraitItem(item) = ident.impl_kind {
            return self.resolve_trait_item_expr(expr_id, typ, item.id());
        }

        // Ensure all instantiation bindings are bound.
        // This ensures even unused type variables like `fn foo<T>() {}` have concrete types
        if let Some(bindings) = self.interner.try_get_instantiation_bindings(expr_id) {
            for (_, kind, binding) in bindings.values() {
                match kind {
                    Kind::Any => (),
                    Kind::Normal => (),
                    Kind::Integer => (),
                    Kind::IntegerOrField => (),
                    Kind::Numeric(typ) => Self::check_type(typ, ident.location)?,
                }
                Self::check_type(binding, ident.location)?;
            }
        }

        let definition = self.interner.definition(ident.id);
        match &definition.kind {
            DefinitionKind::Function(func_id) => {
                let mutable = definition.mutable;
                let location = Some(ident.location);
                let name = definition.name.clone();
                let definition = self.lookup_function(
                    *func_id,
                    expr_id,
                    &typ,
                    &generics.unwrap_or_default(),
                    None,
                );
                let typ = Self::convert_type(&typ, ident.location)?;
                let id = self.next_ident_id();
                let ident =
                    ast::Ident { location, mutable, definition, name, typ: typ.clone(), id };
                let ident_expression = ast::Expression::Ident(ident);
                if self.is_function_closure_type(&typ) {
                    let ident_clone = Box::new(ident_expression.clone());
                    let function = ast::Expression::ExtractTupleField(ident_clone, 0);
                    let env = ast::Expression::ExtractTupleField(Box::new(ident_expression), 1);
                    Ok(ast::Expression::Tuple(vec![function, env]))
                } else {
                    Ok(ident_expression)
                }
            }
            DefinitionKind::Global(global_id) => {
                self.global_ident(*global_id, definition.name.clone(), &typ, ident.location)
            }
            DefinitionKind::Local(_) => match self.lookup_captured_expr(ident.id) {
                Some(expr) => Ok(expr),
                None => {
                    let Some(ident) = self.local_ident(&ident, &typ)? else {
                        let location = self.interner.id_location(expr_id);
                        let message = "ICE: Variable not found during monomorphization";
                        return Err(MonomorphizationError::InternalError { location, message });
                    };
                    Ok(ast::Expression::Ident(ident))
                }
            },
            DefinitionKind::NumericGeneric(type_variable, numeric_typ) => {
                let location = self.interner.id_location(expr_id);
                let value = Type::TypeVariable(type_variable.clone());
                self.numeric_generic(value, numeric_typ.as_ref().clone(), typ, location)
            }
            DefinitionKind::AssociatedConstant(trait_impl_id, name) => {
                let location = ident.location;
                let associated_types = self.interner.get_associated_types_for_impl(*trait_impl_id);
                let associated_type = associated_types
                    .iter()
                    .find(|typ| typ.name.as_str() == name)
                    .expect("Expected to find associated type");
                let Kind::Numeric(numeric_type) = associated_type.typ.kind() else {
                    unreachable!("Expected associated type to be numeric");
                };
                match associated_type
                    .typ
                    .evaluate_to_field_element(&associated_type.typ.kind(), location)
                {
                    Ok(value) => {
                        let typ = Self::convert_type(&numeric_type, location)?;
                        let value = SignedField::positive(value);
                        Ok(ast::Expression::Literal(ast::Literal::Integer(value, typ, location)))
                    }
                    Err(err) => Err(MonomorphizationError::CannotComputeAssociatedConstant {
                        name: name.clone(),
                        err,
                        location,
                    }),
                }
            }
        }
    }

    /// Monomorphize a numeric generic as a numeric constant.
    /// Expects arguments to correspond to `let N: $expected_type = $value;`
    fn numeric_generic(
        &self,
        value: Type,
        expected_type: Type,
        expr_type: Type,
        location: Location,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let expected_kind = Kind::Numeric(Box::new(expected_type.clone()));
        let value = value.evaluate_to_field_element(&expected_kind, location).map_err(|err| {
            MonomorphizationError::UnknownArrayLength {
                length: value.follow_bindings(),
                err,
                location,
            }
        })?;

        let expr_kind = Kind::Numeric(Box::new(expr_type.clone()));
        if !expected_kind.unifies(&expr_kind) {
            let message = "ICE: Generic's kind does not match expected type";
            return Err(MonomorphizationError::InternalError { location, message });
        }

        let typ = Self::convert_type(&expected_type, location)?;
        let value = SignedField::positive(value);
        Ok(ast::Expression::Literal(ast::Literal::Integer(value, typ, location)))
    }

    fn global_ident(
        &mut self,
        global_id: node_interner::GlobalId,
        name: String,
        typ: &HirType,
        location: Location,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let global = self.interner.get_global(global_id);
        let id = global.id;
        let expr = if let Some(seen_global) = self.globals.get(&id) {
            let typ = Self::convert_type(typ, location)?;
            let ident = ast::Ident {
                location: Some(location),
                definition: Definition::Global(*seen_global),
                mutable: false,
                name,
                typ,
                id: self.next_ident_id(),
            };
            ast::Expression::Ident(ident)
        } else {
            let (expr, contains_function) = if let GlobalValue::Resolved(value) =
                global.value.clone()
            {
                let contains_function = value.contains_function_or_closure();
                let expr = value
                    .into_hir_expression(self.interner, global.location)
                    .map_err(MonomorphizationError::InterpreterError)?;
                (expr, contains_function)
            } else {
                unreachable!(
                    "All global values should be resolved at compile time and before monomorphization"
                );
            };

            let expr = self.expr(expr)?;

            // Globals are meant to be computed at compile time and are stored in their own context to be shared across functions.
            // Closures are defined as normal functions among all SSA functions and later need to be defunctionalized.
            // Thus, this means we would have to re-define any global closures.
            // The effect of defunctionalization would be the same if we were redefining a global closure or a local closure
            // just with an extra step of indirection through a global variable.
            // For simplicity, we chose to instead inline closures at their callsite as we do not expect
            // placing a closure in the global context to change the final result of the program.
            if !contains_function {
                let new_id = self.next_global_id();
                self.globals.insert(id, new_id);
                let typ = Self::convert_type(typ, location)?;
                self.finished_globals.insert(new_id, (name.clone(), typ.clone(), expr));
                let ident = ast::Ident {
                    location: Some(location),
                    definition: Definition::Global(new_id),
                    mutable: false,
                    name,
                    typ,
                    id: self.next_ident_id(),
                };
                ast::Expression::Ident(ident)
            } else {
                expr
            }
        };
        Ok(expr)
    }

    /// Convert a non-tuple/struct type to a monomorphized type
    fn convert_type(typ: &HirType, location: Location) -> Result<ast::Type, MonomorphizationError> {
        Self::convert_type_helper(typ, location, &mut HashSet::default())
    }

    fn convert_type_helper(
        typ: &HirType,
        location: Location,
        seen_types: &mut HashSet<Type>,
    ) -> Result<ast::Type, MonomorphizationError> {
        let typ = typ.follow_bindings_shallow();
        Ok(match typ.as_ref() {
            HirType::FieldElement => ast::Type::Field,
            HirType::Integer(sign, bits) => ast::Type::Integer(*sign, *bits),
            HirType::Bool => ast::Type::Bool,
            HirType::String(size) => {
                let size = match size.evaluate_to_u32(location) {
                    Ok(size) => size,
                    // only default variable sizes to size 0
                    Err(TypeCheckError::NonConstantEvaluated { .. }) => 0,
                    Err(err) => {
                        let length = size.as_ref().clone();
                        return Err(MonomorphizationError::UnknownArrayLength {
                            location,
                            err,
                            length,
                        });
                    }
                };
                ast::Type::String(size)
            }
            HirType::FmtString(size, fields) => {
                let size = match size.evaluate_to_u32(location) {
                    Ok(size) => size,
                    // only default variable sizes to size 0
                    Err(TypeCheckError::NonConstantEvaluated { .. }) => 0,
                    Err(err) => {
                        let length = size.as_ref().clone();
                        return Err(MonomorphizationError::UnknownArrayLength {
                            location,
                            err,
                            length,
                        });
                    }
                };
                let fields =
                    Box::new(Self::convert_type_helper(fields.as_ref(), location, seen_types)?);
                ast::Type::FmtString(size, fields)
            }
            HirType::Unit => ast::Type::Unit,
            HirType::Array(length, element) => {
                let element =
                    Box::new(Self::convert_type_helper(element.as_ref(), location, seen_types)?);
                let length = match length.evaluate_to_u32(location) {
                    Ok(length) => length,
                    Err(err) => {
                        let length = length.as_ref().clone();
                        return Err(MonomorphizationError::UnknownArrayLength {
                            location,
                            err,
                            length,
                        });
                    }
                };
                ast::Type::Array(length, element)
            }
            HirType::Slice(element) => {
                let element =
                    Box::new(Self::convert_type_helper(element.as_ref(), location, seen_types)?);
                ast::Type::Slice(element)
            }
            HirType::TraitAsType(..) => {
                unreachable!("All TraitAsType should be replaced before calling convert_type");
            }
            HirType::NamedGeneric(NamedGeneric { type_var, .. }) => {
                if let TypeBinding::Bound(binding) = &*type_var.borrow() {
                    return Self::convert_type_helper(binding, location, seen_types);
                }

                // Default any remaining unbound type variables.
                // This should only happen if the variable in question is unused
                // and within a larger generic type.
                type_var.bind(HirType::default_int_or_field_type());
                ast::Type::Field
            }

            HirType::CheckedCast { from, to } => {
                Self::check_checked_cast(from, to, location)?;
                Self::convert_type_helper(to, location, seen_types)?
            }

            HirType::TypeVariable(binding) => {
                let input_type = typ.as_ref().clone();
                if !seen_types.insert(input_type.clone()) {
                    let typ = input_type;
                    return Err(MonomorphizationError::RecursiveType { typ, location });
                }

                let type_var_kind = match &*binding.borrow() {
                    TypeBinding::Bound(binding) => {
                        let typ = Self::convert_type_helper(binding, location, seen_types);
                        seen_types.remove(&input_type);
                        return typ;
                    }
                    TypeBinding::Unbound(_, type_var_kind) => type_var_kind.clone(),
                };

                // Default any remaining unbound type variables.
                // This should only happen if the variable in question is unused
                // and within a larger generic type.
                let default = match type_var_kind.default_type() {
                    Some(typ) => typ,
                    None => {
                        return Err(MonomorphizationError::NoDefaultType { location });
                    }
                };

                let monomorphized_default =
                    Self::convert_type_helper(&default, location, seen_types)?;
                binding.bind(default);
                monomorphized_default
            }

            HirType::DataType(def, args) => {
                // Not all generic arguments may be used in a datatype's fields so we have to check
                // the arguments as well as the fields in case any need to be defaulted or are unbound.
                for arg in args {
                    Self::check_type(arg, location)?;
                }

                let input_type = typ.as_ref().clone();
                if !seen_types.insert(input_type.clone()) {
                    let typ = input_type;
                    return Err(MonomorphizationError::RecursiveType { typ, location });
                }

                let def = def.borrow();
                if let Some(fields) = def.get_fields(args) {
                    let fields = try_vecmap(fields, |(_, field, _)| {
                        Self::convert_type_helper(&field, location, seen_types)
                    })?;

                    seen_types.remove(&input_type);
                    ast::Type::Tuple(fields)
                } else if let Some(variants) = def.get_variants(args) {
                    // Enums are represented as (tag, variant1, variant2, .., variantN)
                    let mut fields = vec![ast::Type::Field];
                    for (_, variant_fields) in variants {
                        let variant_fields = try_vecmap(variant_fields, |typ| {
                            Self::convert_type_helper(&typ, location, seen_types)
                        })?;
                        fields.push(ast::Type::Tuple(variant_fields));
                    }
                    seen_types.remove(&input_type);
                    ast::Type::Tuple(fields)
                } else {
                    unreachable!("Data type has no body")
                }
            }

            HirType::Alias(def, args) => {
                // Similar to the struct case above: generics of an alias might not end up being
                // used in the type that is aliased.
                for arg in args {
                    Self::check_type(arg, location)?;
                }

                Self::convert_type_helper(&def.borrow().get_type(args), location, seen_types)?
            }

            HirType::Tuple(fields) => {
                let fields =
                    try_vecmap(fields, |x| Self::convert_type_helper(x, location, seen_types))?;
                ast::Type::Tuple(fields)
            }

            HirType::Function(args, ret, env, unconstrained) => {
                let args =
                    try_vecmap(args, |x| Self::convert_type_helper(x, location, seen_types))?;
                let ret = Box::new(Self::convert_type_helper(ret, location, seen_types)?);
                let env = Self::convert_type_helper(env, location, seen_types)?;
                match &env {
                    ast::Type::Unit => {
                        ast::Type::Function(args, ret, Box::new(env), *unconstrained)
                    }
                    ast::Type::Tuple(_elements) => ast::Type::Tuple(vec![
                        env.clone(),
                        ast::Type::Function(args, ret, Box::new(env), *unconstrained),
                    ]),
                    _ => {
                        unreachable!(
                            "internal Type::Function env should be either a Unit or a Tuple, not {env}"
                        )
                    }
                }
            }

            // Lower both mutable & immutable references to the same reference type
            HirType::Reference(element, mutable) => {
                let element = Self::convert_type_helper(element, location, seen_types)?;
                ast::Type::Reference(Box::new(element), *mutable)
            }

            HirType::Forall(_, _) | HirType::Constant(..) | HirType::InfixExpr(..) => {
                unreachable!("Unexpected type {typ} found")
            }
            HirType::Error => {
                let message = "Unexpected Type::Error found during monomorphization";
                return Err(MonomorphizationError::InternalError { message, location });
            }
            HirType::Quoted(typ) => {
                let typ = typ.to_string();
                return Err(MonomorphizationError::ComptimeTypeInRuntimeCode { typ, location });
            }
        })
    }

    // Similar to `convert_type` but returns an error if any type variable can't be defaulted.
    fn check_type(typ: &HirType, location: Location) -> Result<(), MonomorphizationError> {
        let typ = typ.follow_bindings_shallow();
        match typ.as_ref() {
            HirType::FieldElement
            | HirType::Integer(..)
            | HirType::Bool
            | HirType::String(..)
            | HirType::Unit
            | HirType::TraitAsType(..)
            | HirType::Forall(_, _)
            | HirType::Error
            | HirType::Quoted(_) => Ok(()),
            HirType::Constant(_value, kind) => {
                if kind.is_error() {
                    Err(MonomorphizationError::UnknownConstant { location })
                } else {
                    Ok(())
                }
            }
            HirType::CheckedCast { from, to } => {
                Self::check_checked_cast(from, to, location)?;
                Self::check_type(to, location)
            }

            HirType::FmtString(_size, fields) => Self::check_type(fields.as_ref(), location),
            HirType::Array(_length, element) => Self::check_type(element.as_ref(), location),
            HirType::Slice(element) => Self::check_type(element.as_ref(), location),
            HirType::NamedGeneric(NamedGeneric { type_var, .. }) => {
                if let TypeBinding::Bound(binding) = &*type_var.borrow() {
                    return Self::check_type(binding, location);
                }

                Ok(())
            }
            HirType::TypeVariable(binding) => {
                let type_var_kind = match &*binding.borrow() {
                    TypeBinding::Bound(binding) => {
                        return Self::check_type(binding, location);
                    }
                    TypeBinding::Unbound(_, type_var_kind) => type_var_kind.clone(),
                };

                // Default any remaining unbound type variables.
                // This should only happen if the variable in question is unused
                // and within a larger generic type.
                let default = match type_var_kind.default_type() {
                    Some(typ) => typ,
                    None => {
                        return Err(MonomorphizationError::NoDefaultType { location });
                    }
                };

                Self::check_type(&default, location)
            }

            HirType::DataType(_def, args) => {
                for arg in args {
                    Self::check_type(arg, location)?;
                }

                Ok(())
            }

            HirType::Alias(_def, args) => {
                for arg in args {
                    Self::check_type(arg, location)?;
                }

                Ok(())
            }

            HirType::Tuple(fields) => {
                for field in fields {
                    Self::check_type(field, location)?;
                }

                Ok(())
            }

            HirType::Function(args, ret, env, _) => {
                for arg in args {
                    Self::check_type(arg, location)?;
                }

                Self::check_type(ret, location)?;
                Self::check_type(env, location)
            }

            HirType::Reference(element, _mutable) => Self::check_type(element, location),
            HirType::InfixExpr(lhs, _, rhs, _) => {
                Self::check_type(lhs, location)?;
                Self::check_type(rhs, location)
            }
        }
    }

    /// Check that the 'from' and to' sides of a CheckedCast unify and
    /// that if the 'to' side evaluates to a field element, that the 'from' side
    /// evaluates to the same field element
    fn check_checked_cast(
        from: &Type,
        to: &Type,
        location: Location,
    ) -> Result<(), MonomorphizationError> {
        if from.unify(to).is_err() {
            return Err(MonomorphizationError::CheckedCastFailed {
                actual: to.clone(),
                expected: from.clone(),
                location,
            });
        }
        let to_value = to.evaluate_to_field_element(&to.kind(), location);
        if to_value.is_ok() {
            let skip_simplifications = false;
            let from_value =
                from.evaluate_to_field_element_helper(&to.kind(), location, skip_simplifications);
            if from_value.is_err() || from_value.unwrap() != to_value.clone().unwrap() {
                return Err(MonomorphizationError::CheckedCastFailed {
                    actual: HirType::Constant(to_value.unwrap(), to.kind()),
                    expected: from.clone(),
                    location,
                });
            }
        }
        Ok(())
    }

    fn is_function_closure(&self, t: ast::Type) -> bool {
        if self.is_function_closure_type(&t) {
            true
        } else if let ast::Type::Tuple(elements) = t {
            if elements.len() == 2 {
                matches!(elements[1], ast::Type::Function(_, _, _, _))
            } else {
                false
            }
        } else {
            false
        }
    }

    fn is_function_closure_type(&self, t: &ast::Type) -> bool {
        if let ast::Type::Function(_, _, env, _) = t {
            let e = (*env).clone();
            matches!(*e, ast::Type::Tuple(_captures))
        } else {
            false
        }
    }

    fn resolve_trait_item_expr(
        &mut self,
        expr_id: node_interner::ExprId,
        function_type: HirType,
        trait_item_id: TraitItemId,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let item = resolve_trait_item(self.interner, trait_item_id, expr_id)
            .map_err(MonomorphizationError::InterpreterError)?;

        let func_id = match item {
            TraitItem::Method(func_id) => func_id,
            TraitItem::Constant { id, expected_type, value } => {
                let location = self.interner.definition(id).location;
                let expr_type = self.interner.id_type(expr_id);
                return self.numeric_generic(value, expected_type, expr_type, location);
            }
        };

        let func_id = match self.lookup_function(
            func_id,
            expr_id,
            &function_type,
            &[],
            Some(trait_item_id),
        ) {
            Definition::Function(func_id) => func_id,
            _ => unreachable!(),
        };

        let location = self.interner.expr_location(&expr_id);
        let name = self.interner.definition_name(trait_item_id.item_id).to_string();

        Ok(ast::Expression::Ident(ast::Ident {
            definition: Definition::Function(func_id),
            mutable: false,
            location: None,
            name,
            typ: Self::convert_type(&function_type, location)?,
            id: self.next_ident_id(),
        }))
    }

    fn function_call(
        &mut self,
        call: HirCallExpression,
        id: node_interner::ExprId,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let original_func = Box::new(self.expr(call.func)?);
        let mut arguments = try_vecmap(&call.arguments, |id| self.expr(*id))?;
        let hir_arguments = vecmap(&call.arguments, |id| self.interner.expression(id));

        self.patch_debug_instrumentation_call(&call, &mut arguments)?;

        let return_type = self.interner.id_type(id);
        let location = self.interner.expr_location(&id);
        let return_type = Self::convert_type(&return_type, location)?;

        let location = call.location;

        if let ast::Expression::Ident(ident) = original_func.as_ref() {
            if let Definition::Oracle(name) = &ident.definition {
                if name.as_str() == "print" {
                    // Oracle calls are required to be wrapped in an unconstrained function
                    // The first argument to the `print` oracle is a bool, indicating a newline to be inserted at the end of the input
                    // The second argument is expected to always be an ident
                    self.append_printable_type_info(&hir_arguments[1], &mut arguments);
                }
            }
            if let Definition::Builtin(name) = &ident.definition {
                if name.as_str() == "static_assert" {
                    // static_assert can take any type for the `message` argument.
                    // Here we append printable type info so we can know how to turn that argument
                    // into a human-readable string.
                    let typ = self.interner.id_type(call.arguments[1]);
                    append_printable_type_info_for_type(typ, &mut arguments);
                }
            }
        }

        let mut block_expressions = vec![];
        let func_type = self.interner.id_type(call.func);
        let func_type = Self::convert_type(&func_type, location)?;
        let is_closure = self.is_function_closure(func_type);

        let func = if is_closure {
            let local_id = self.next_local_id();

            // store the function in a temporary variable before calling it
            // this is needed for example if call.func is of the form `foo()()`
            // without this, we would translate it to `foo().1(foo().0)`
            let let_stmt = ast::Expression::Let(ast::Let {
                id: local_id,
                mutable: false,
                name: "tmp".to_string(),
                expression: Box::new(*original_func),
            });
            block_expressions.push(let_stmt);

            let extracted_func = ast::Expression::Ident(ast::Ident {
                location: None,
                definition: Definition::Local(local_id),
                mutable: false,
                name: "tmp".to_string(),
                typ: Self::convert_type(&self.interner.id_type(call.func), location)?,
                id: self.next_ident_id(),
            });

            let env_argument =
                ast::Expression::ExtractTupleField(Box::new(extracted_func.clone()), 0usize);
            arguments.insert(0, env_argument);

            Box::new(ast::Expression::ExtractTupleField(Box::new(extracted_func), 1usize))
        } else {
            original_func.clone()
        };

        let call = self
            .try_evaluate_call(&func, &id, &call.arguments, &arguments, &return_type)?
            .unwrap_or(ast::Expression::Call(ast::Call { func, arguments, return_type, location }));

        if !block_expressions.is_empty() {
            block_expressions.push(call);
            Ok(ast::Expression::Block(block_expressions))
        } else {
            Ok(call)
        }
    }

    /// Adds a function argument that contains type metadata that is required to tell
    /// `println` how to convert values passed to an foreign call back to a human-readable string.
    /// The values passed to an foreign call will be a simple list of field elements,
    /// thus requiring extra metadata to correctly decode this list of elements.
    ///
    /// The Noir compiler has a `PrintableType` that handles encoding/decoding a list
    /// of field elements to/from JSON. The type metadata attached in this method
    /// is the serialized `PrintableType` for the argument passed to the function.
    /// The caller that is running a Noir program should then deserialize the `PrintableType`,
    /// and accurately decode the list of field elements passed to the foreign call.
    fn append_printable_type_info(
        &mut self,
        hir_argument: &HirExpression,
        arguments: &mut Vec<ast::Expression>,
    ) {
        match hir_argument {
            HirExpression::Ident(ident, _) => {
                let typ = self.interner.definition_type(ident.id);
                append_printable_type_info_for_type(typ, arguments);
            }
            _ => unreachable!("logging expr {:?} is not supported", hir_argument),
        }
    }

    /// Try to evaluate certain builtin functions (currently only 'array_len' and field modulus methods)
    /// at their call site.
    /// NOTE: Evaluating at the call site means we cannot track aliased functions.
    ///       E.g. `let f = std::array::len; f(arr)` will fail to evaluate.
    ///       To fix this we need to evaluate on the identifier instead, which
    ///       requires us to evaluate to a Lambda value which isn't in noir yet.
    fn try_evaluate_call(
        &mut self,
        func: &ast::Expression,
        expr_id: &node_interner::ExprId,
        arguments: &[node_interner::ExprId],
        argument_values: &[ast::Expression],
        result_type: &ast::Type,
    ) -> Result<Option<ast::Expression>, MonomorphizationError> {
        if let ast::Expression::Ident(ident) = func {
            if let Definition::Builtin(opcode) = &ident.definition {
                // TODO(#1736): Move this builtin to the SSA pass
                let location = self.interner.expr_location(expr_id);
                return Ok(match opcode.as_str() {
                    "modulus_num_bits" => {
                        let bits = FieldElement::max_num_bits();
                        let typ =
                            ast::Type::Integer(Signedness::Unsigned, IntegerBitSize::SixtyFour);
                        let bits = SignedField::positive(bits);
                        Some(ast::Expression::Literal(ast::Literal::Integer(bits, typ, location)))
                    }
                    "zeroed" => {
                        let location = self.interner.expr_location(expr_id);
                        Some(self.zeroed_value_of_type(result_type, location))
                    }
                    "modulus_le_bits" => {
                        let bits = FieldElement::modulus().to_radix_le(2);
                        Some(self.modulus_slice_literal(bits, IntegerBitSize::One, location))
                    }
                    "modulus_be_bits" => {
                        let bits = FieldElement::modulus().to_radix_be(2);
                        Some(self.modulus_slice_literal(bits, IntegerBitSize::One, location))
                    }
                    "modulus_be_bytes" => {
                        let bytes = FieldElement::modulus().to_bytes_be();
                        Some(self.modulus_slice_literal(bytes, IntegerBitSize::Eight, location))
                    }
                    "modulus_le_bytes" => {
                        let bytes = FieldElement::modulus().to_bytes_le();
                        Some(self.modulus_slice_literal(bytes, IntegerBitSize::Eight, location))
                    }
                    "checked_transmute" => {
                        Some(self.checked_transmute(*expr_id, arguments, argument_values)?)
                    }
                    _ => None,
                });
            }
        }
        Ok(None)
    }

    fn checked_transmute(
        &mut self,
        expr_id: node_interner::ExprId,
        arguments: &[node_interner::ExprId],
        argument_values: &[ast::Expression],
    ) -> Result<ast::Expression, MonomorphizationError> {
        let location = self.interner.expr_location(&expr_id);
        let actual = self.interner.id_type(arguments[0]).follow_bindings();
        let expected = self.interner.id_type(expr_id).follow_bindings();

        if actual.unify(&expected).is_err() {
            Err(MonomorphizationError::CheckedTransmuteFailed { actual, expected, location })
        } else {
            // Evaluate `checked_transmute(arg)` to `{ arg }`
            // in case the user did `&mut checked_transmute(arg)`. Wrapping the
            // arg in a block prevents mutating the original argument.
            let argument = argument_values[0].clone();
            Ok(ast::Expression::Block(vec![argument]))
        }
    }

    fn modulus_slice_literal(
        &self,
        bytes: Vec<u8>,
        arr_elem_bits: IntegerBitSize,
        location: Location,
    ) -> ast::Expression {
        use ast::*;

        let int_type = Type::Integer(Signedness::Unsigned, arr_elem_bits);

        let bytes_as_expr = vecmap(bytes, |byte| {
            let value = SignedField::positive(byte as u32);
            Expression::Literal(Literal::Integer(value, int_type.clone(), location))
        });

        let typ = Type::Slice(Box::new(int_type));
        let arr_literal = ArrayLiteral { typ, contents: bytes_as_expr };
        Expression::Literal(Literal::Slice(arr_literal))
    }

    fn queue_function(
        &mut self,
        id: node_interner::FuncId,
        expr_id: node_interner::ExprId,
        function_type: HirType,
        turbofish_generics: Vec<HirType>,
        trait_method: Option<TraitItemId>,
    ) -> FuncId {
        let new_id = self.next_function_id();
        let is_unconstrained = self.is_unconstrained(id);
        self.define_function(
            id,
            function_type.clone(),
            turbofish_generics,
            is_unconstrained,
            new_id,
        );

        let location = self.interner.expr_location(&expr_id);
        let bindings = self.interner.get_instantiation_bindings(expr_id);
        let bindings = self.follow_bindings(bindings);
        self.queue.push_back((id, new_id, bindings, trait_method, is_unconstrained, location));
        new_id
    }

    /// Follow any type variable links within the given TypeBindings to produce
    /// a new TypeBindings that won't be changed when bindings are pushed or popped
    /// during {perform,undo}_monomorphization_bindings.
    ///
    /// Without this, a monomorphized type may fail to propagate passed more than 2
    /// function calls deep since it is possible for a previous link in the chain to
    /// unbind a type variable that was previously bound.
    fn follow_bindings(&self, bindings: &TypeBindings) -> TypeBindings {
        bindings
            .iter()
            .map(|(id, (var, kind, binding))| {
                let binding2 = binding.follow_bindings();
                (*id, (var.clone(), kind.clone(), binding2))
            })
            .collect()
    }

    fn assign(
        &mut self,
        assign: HirAssignStatement,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let expression_type = self.interner.id_type(assign.expression);
        let location = self.interner.expr_location(&assign.expression);
        if !self.in_unconstrained_function && Self::contains_reference(&expression_type) {
            let typ = expression_type.to_string();
            return Err(MonomorphizationError::AssignedToVarContainingReference { typ, location });
        }

        let expression = Box::new(self.expr(assign.expression)?);
        let lvalue = self.lvalue(assign.lvalue)?;
        Ok(ast::Expression::Assign(ast::Assign { expression, lvalue }))
    }

    fn lvalue(&mut self, lvalue: HirLValue) -> Result<ast::LValue, MonomorphizationError> {
        let value = match lvalue {
            HirLValue::Ident(ident, typ) => match self.lookup_captured_lvalue(ident.id) {
                Some(value) => value,
                None => ast::LValue::Ident(self.local_ident(&ident, &typ)?.unwrap()),
            },
            HirLValue::MemberAccess { object, field_index, .. } => {
                let field_index = field_index.unwrap();
                let object = Box::new(self.lvalue(*object)?);
                ast::LValue::MemberAccess { object, field_index }
            }
            HirLValue::Index { array, index, typ, location } => {
                let array = Box::new(self.lvalue(*array)?);
                let index = Box::new(self.expr(index)?);
                let element_type = Self::convert_type(&typ, location)?;
                ast::LValue::Index { array, index, element_type, location }
            }
            HirLValue::Dereference { lvalue, element_type, location, implicitly_added: _ } => {
                let reference = Box::new(self.lvalue(*lvalue)?);
                let element_type = Self::convert_type(&element_type, location)?;
                ast::LValue::Dereference { reference, element_type }
            }
        };

        Ok(value)
    }

    fn lambda(
        &mut self,
        lambda: HirLambda,
        expr: node_interner::ExprId,
    ) -> Result<ast::Expression, MonomorphizationError> {
        if lambda.captures.is_empty() {
            self.lambda_no_capture(lambda, expr)
        } else {
            let (setup, closure_variable) = self.lambda_with_setup(lambda, expr)?;
            Ok(ast::Expression::Block(vec![setup, closure_variable]))
        }
    }

    fn lambda_no_capture(
        &mut self,
        lambda: HirLambda,
        expr: node_interner::ExprId,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let location = self.interner.expr_location(&expr);
        let ret_type = Self::convert_type(&lambda.return_type, location)?;
        let lambda_name = "lambda";
        let parameter_types =
            try_vecmap(&lambda.parameters, |(_, typ)| Self::convert_type(typ, location))?;

        // Manually convert to Parameters type so we can reuse the self.parameters method
        let parameters =
            vecmap(lambda.parameters, |(pattern, typ)| (pattern, typ, Visibility::Private)).into();

        let parameters = self.parameters(&parameters)?;
        let body = self.expr(lambda.body)?;
        let id = self.next_function_id();

        let function = ast::Function {
            id,
            name: lambda_name.to_owned(),
            parameters,
            body,
            return_type: ret_type.clone(),
            return_visibility: Visibility::Private,
            unconstrained: self.in_unconstrained_function,
            inline_type: InlineType::default(),
            func_sig: FunctionSignature::default(),
        };
        self.push_function(id, function);

        let typ = ast::Type::Function(
            parameter_types,
            Box::new(ret_type),
            Box::new(ast::Type::Unit),
            false,
        );

        let name = lambda_name.to_owned();
        Ok(ast::Expression::Ident(ast::Ident {
            definition: Definition::Function(id),
            mutable: false,
            location: None,
            name,
            typ,
            id: self.next_ident_id(),
        }))
    }

    fn lambda_with_setup(
        &mut self,
        lambda: HirLambda,
        expr: node_interner::ExprId,
    ) -> Result<(ast::Expression, ast::Expression), MonomorphizationError> {
        // returns (<closure setup>, <closure variable>)
        //   which can be used directly in callsites or transformed
        //   directly to a single `Expression`
        // for other cases by `lambda` which is called by `expr`
        //
        // it solves the problem of detecting special cases where
        // we call something like
        // `{let env$.. = ..;}.1({let env$.. = ..;}.0, ..)`
        // which was leading to redefinition errors
        //
        // instead of detecting and extracting
        // patterns in the resulting tree,
        // which seems more fragile, we directly reuse the return parameters
        // of this function in those cases
        let location = self.interner.expr_location(&expr);
        let ret_type = Self::convert_type(&lambda.return_type, location)?;
        let lambda_name = "lambda";
        let parameter_types =
            try_vecmap(&lambda.parameters, |(_, typ)| Self::convert_type(typ, location))?;

        // Manually convert to Parameters type so we can reuse the self.parameters method
        let parameters =
            vecmap(lambda.parameters, |(pattern, typ)| (pattern, typ, Visibility::Private)).into();

        let mut converted_parameters = self.parameters(&parameters)?;

        let id = self.next_function_id();
        let name = lambda_name.to_owned();
        let return_type = ret_type.clone();

        let env_local_id = self.next_local_id();
        let env_name = "env";
        let env_tuple =
            ast::Expression::Tuple(try_vecmap(&lambda.captures, |capture| {
                match capture.transitive_capture_index {
                    Some(field_index) => {
                        let lambda_ctx = self.lambda_envs_stack.last().expect(
                            "Expected to find a parent closure environment, but found none",
                        );

                        let ident = Box::new(ast::Expression::Ident(lambda_ctx.env_ident.clone()));
                        Ok(ast::Expression::ExtractTupleField(ident, field_index))
                    }
                    None => {
                        let typ = self.interner.definition_type(capture.ident.id);
                        let ident = self.local_ident(&capture.ident, &typ)?.unwrap();
                        Ok(ast::Expression::Ident(ident))
                    }
                }
            })?);

        let expr_type = self.interner.id_type(expr);
        let env_typ = if let types::Type::Function(_, _, function_env_type, _) = expr_type {
            Self::convert_type(&function_env_type, location)?
        } else {
            unreachable!("expected a Function type for a Lambda node")
        };

        let env_let_stmt = ast::Expression::Let(ast::Let {
            id: env_local_id,
            mutable: false,
            name: env_name.to_string(),
            expression: Box::new(env_tuple),
        });

        let location = None; // TODO: This should match the location of the lambda expression
        let mutable = true;
        let definition = Definition::Local(env_local_id);

        let env_ident = ast::Ident {
            location,
            mutable,
            definition,
            name: env_name.to_string(),
            typ: env_typ.clone(),
            id: self.next_ident_id(),
        };

        self.lambda_envs_stack
            .push(LambdaContext { env_ident: env_ident.clone(), captures: lambda.captures });
        let body = self.expr(lambda.body)?;
        self.lambda_envs_stack.pop();

        let lambda_fn_typ: ast::Type = ast::Type::Function(
            parameter_types,
            Box::new(ret_type),
            Box::new(env_typ.clone()),
            false,
        );
        let lambda_fn = ast::Expression::Ident(ast::Ident {
            definition: Definition::Function(id),
            mutable: false,
            location: None, // TODO: This should match the location of the lambda expression
            name: name.clone(),
            typ: lambda_fn_typ.clone(),
            id: self.next_ident_id(),
        });

        let mut parameters =
            vec![(env_local_id, true, env_name.to_string(), env_typ.clone(), Visibility::Private)];
        parameters.append(&mut converted_parameters);

        let function = ast::Function {
            id,
            name,
            parameters,
            body,
            return_type,
            return_visibility: Visibility::Private,
            unconstrained: self.in_unconstrained_function,
            inline_type: InlineType::default(),
            func_sig: FunctionSignature::default(),
        };
        self.push_function(id, function);

        let lambda_value =
            ast::Expression::Tuple(vec![ast::Expression::Ident(env_ident), lambda_fn]);
        let block_local_id = self.next_local_id();
        let block_ident_name = "closure_variable";
        let block_let_stmt = ast::Expression::Let(ast::Let {
            id: block_local_id,
            mutable: false,
            name: block_ident_name.to_string(),
            expression: Box::new(ast::Expression::Block(vec![env_let_stmt, lambda_value])),
        });

        let closure_definition = Definition::Local(block_local_id);

        let closure_ident = ast::Expression::Ident(ast::Ident {
            location,
            mutable: false,
            definition: closure_definition,
            name: block_ident_name.to_string(),
            typ: ast::Type::Tuple(vec![env_typ, lambda_fn_typ]),
            id: self.next_ident_id(),
        });

        Ok((block_let_stmt, closure_ident))
    }

    fn match_expr(
        &mut self,
        match_expr: HirMatch,
        expr_id: ExprId,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let result_type = self.interner.id_type(expr_id);
        let location = self.interner.expr_location(&expr_id);

        if !self.in_unconstrained_function && Self::contains_reference(&result_type) {
            let typ = result_type.to_string();
            return Err(MonomorphizationError::ReferenceReturnedFromIfOrMatch { typ, location });
        }

        match match_expr {
            HirMatch::Success(id) => self.expr(id),
            HirMatch::Failure { .. } => {
                let false_ = Box::new(ast::Expression::Literal(ast::Literal::Bool(false)));
                let msg = "match failure";
                let msg_expr = ast::Expression::Literal(ast::Literal::Str(msg.to_string()));

                let u32_type = HirType::Integer(Signedness::Unsigned, IntegerBitSize::ThirtyTwo);
                let length = (msg.len() as u128).into();
                let length = HirType::Constant(length, Kind::Numeric(Box::new(u32_type)));
                let msg_type = HirType::String(Box::new(length));

                let msg = Some(Box::new((msg_expr, msg_type)));
                Ok(ast::Expression::Constrain(false_, location, msg))
            }
            HirMatch::Guard { cond, body, otherwise } => {
                let condition = Box::new(self.expr(cond)?);
                let consequence = Box::new(self.expr(body)?);
                let alternative = Some(Box::new(self.match_expr(*otherwise, expr_id)?));
                let typ = Self::convert_type(&result_type, location)?;
                Ok(ast::Expression::If(ast::If { condition, consequence, alternative, typ }))
            }
            HirMatch::Switch(variable_to_match, cases, default) => {
                let variable_name = self.interner.definition(variable_to_match).name.clone();
                let variable_id = match self.lookup_local(variable_to_match) {
                    Some(Definition::Local(id)) => id,
                    other => unreachable!("Expected match variable to be defined. Found {other:?}"),
                };

                let cases = try_vecmap(cases, |case| {
                    let arguments = vecmap(case.arguments, |arg| {
                        let arg_name = self.interner.definition(arg).name.clone();
                        let new_id = self.next_local_id();
                        self.define_local(arg, new_id);
                        (new_id, arg_name)
                    });
                    let branch = self.match_expr(case.body, expr_id)?;
                    Ok(ast::MatchCase { constructor: case.constructor, arguments, branch })
                })?;

                let default_case = match default {
                    Some(case) => Some(Box::new(self.match_expr(*case, expr_id)?)),
                    None => None,
                };

                let typ = Self::convert_type(&result_type, location)?;
                Ok(ast::Expression::Match(ast::Match {
                    variable_to_match: (variable_id, variable_name),
                    cases,
                    default_case,
                    typ,
                }))
            }
        }
    }

    /// Implements std::unsafe_func::zeroed by returning an appropriate zeroed
    /// ast literal or collection node for the given type. Note that for functions
    /// there is no obvious zeroed value so this should be considered unsafe to use.
    fn zeroed_value_of_type(
        &mut self,
        typ: &ast::Type,
        location: noirc_errors::Location,
    ) -> ast::Expression {
        match typ {
            ast::Type::Field | ast::Type::Integer(..) => {
                let typ = typ.clone();
                let zero = SignedField::positive(0u32);
                ast::Expression::Literal(ast::Literal::Integer(zero, typ, location))
            }
            ast::Type::Bool => ast::Expression::Literal(ast::Literal::Bool(false)),
            ast::Type::Unit => ast::Expression::Literal(ast::Literal::Unit),
            ast::Type::Array(length, element_type) => {
                let element = self.zeroed_value_of_type(element_type.as_ref(), location);
                ast::Expression::Literal(ast::Literal::Array(ast::ArrayLiteral {
                    contents: vec![element; *length as usize],
                    typ: ast::Type::Array(*length, element_type.clone()),
                }))
            }
            ast::Type::String(length) => {
                ast::Expression::Literal(ast::Literal::Str("\0".repeat(*length as usize)))
            }
            ast::Type::FmtString(length, fields) => {
                let zeroed_tuple = self.zeroed_value_of_type(fields, location);
                let fields_len = match &zeroed_tuple {
                    ast::Expression::Tuple(fields) => fields.len() as u64,
                    _ => unreachable!(
                        "ICE: format string fields should be structured in a tuple, but got a {zeroed_tuple}"
                    ),
                };
                ast::Expression::Literal(ast::Literal::FmtStr(
                    vec![FmtStrFragment::String("\0".repeat(*length as usize))],
                    fields_len,
                    Box::new(zeroed_tuple),
                ))
            }
            ast::Type::Tuple(fields) => ast::Expression::Tuple(vecmap(fields, |field| {
                self.zeroed_value_of_type(field, location)
            })),
            ast::Type::Function(parameter_types, ret_type, env, unconstrained) => self
                .create_zeroed_function(parameter_types, ret_type, env, *unconstrained, location),
            ast::Type::Slice(element_type) => {
                ast::Expression::Literal(ast::Literal::Slice(ast::ArrayLiteral {
                    contents: vec![],
                    typ: ast::Type::Slice(element_type.clone()),
                }))
            }
            ast::Type::Reference(element, mutable) => {
                use UnaryOp::Reference;
                let rhs = Box::new(self.zeroed_value_of_type(element, location));
                let result_type = typ.clone();
                ast::Expression::Unary(ast::Unary {
                    rhs,
                    result_type,
                    operator: Reference { mutable: *mutable },
                    location,
                    skip: false,
                })
            }
        }
    }

    // Creating a zeroed function value is almost always an error if it is used later,
    // Hence why std::unsafe_func::zeroed is unsafe.
    //
    // To avoid confusing later passes, we arbitrarily choose to construct a function
    // that satisfies the input type by discarding all its parameters and returning a
    // zeroed value of the result type.
    fn create_zeroed_function(
        &mut self,
        parameter_types: &[ast::Type],
        ret_type: &ast::Type,
        env_type: &ast::Type,
        unconstrained: bool,
        location: noirc_errors::Location,
    ) -> ast::Expression {
        let lambda_name = "zeroed_lambda";

        let parameters = vecmap(parameter_types, |parameter_type| {
            (self.next_local_id(), false, "_".into(), parameter_type.clone(), Visibility::Private)
        });

        let body = self.zeroed_value_of_type(ret_type, location);

        let id = self.next_function_id();
        let return_type = ret_type.clone();
        let name = lambda_name.to_owned();

        let function = ast::Function {
            id,
            name,
            parameters,
            body,
            return_type,
            return_visibility: Visibility::Private,
            unconstrained,
            inline_type: InlineType::default(),
            func_sig: FunctionSignature::default(),
        };
        self.push_function(id, function);

        ast::Expression::Ident(ast::Ident {
            definition: Definition::Function(id),
            mutable: false,
            location: None,
            name: lambda_name.to_owned(),
            typ: ast::Type::Function(
                parameter_types.to_owned(),
                Box::new(ret_type.clone()),
                Box::new(env_type.clone()),
                unconstrained,
            ),
            id: self.next_ident_id(),
        })
    }

    /// Call an infix operator overloading method for the given operator.
    /// This function handles the special cases some operators have which don't map
    /// 1 to 1 onto their operator function. For example: != requires a negation on
    /// the result of its `eq` method, and the comparison operators each require a
    /// conversion from the `Ordering` result to a boolean.
    fn create_infix_operator_impl_call(
        &self,
        func: ast::Expression,
        lhs: ast::Expression,
        operator: HirBinaryOp,
        rhs: ast::Expression,
        ret: Type,
        location: Location,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let arguments = vec![lhs, rhs];
        let func = Box::new(func);
        let return_type = Self::convert_type(&ret, location)?;

        let mut result =
            ast::Expression::Call(ast::Call { func, arguments, return_type, location });

        use crate::ast::BinaryOpKind::*;
        match operator.kind {
            // Negate the result of the == operation
            NotEqual => {
                result = ast::Expression::Unary(ast::Unary {
                    operator: UnaryOp::Not,
                    rhs: Box::new(result),
                    result_type: ast::Type::Bool,
                    location,
                    skip: false,
                });
            }
            // All the comparison operators require special handling since their `cmp` method
            // returns an `Ordering` rather than a boolean value.
            //
            // (a < b) => a.cmp(b) == Ordering::Less
            // (a <= b) => a.cmp(b) != Ordering::Greater
            // (a > b) => a.cmp(b) == Ordering::Greater
            // (a >= b) => a.cmp(b) != Ordering::Less
            Less | LessEqual | Greater | GreaterEqual => {
                // Comparing an Ordering directly to a field value in this way takes advantage
                // of the fact the Ordering struct contains a single Field type, and our SSA
                // pass will automatically unpack tuple values.
                let ordering_value = if matches!(operator.kind, Less | GreaterEqual) {
                    FieldElement::zero() // Ordering::Less
                } else {
                    2u128.into() // Ordering::Greater
                };

                let operator =
                    if matches!(operator.kind, Less | Greater) { Equal } else { NotEqual };

                let ordering_value = SignedField::positive(ordering_value);
                let int_value = ast::Literal::Integer(ordering_value, ast::Type::Field, location);
                let rhs = Box::new(ast::Expression::Literal(int_value));
                let lhs = Box::new(ast::Expression::ExtractTupleField(Box::new(result), 0));

                result = ast::Expression::Binary(ast::Binary { lhs, operator, rhs, location });
            }
            _ => (),
        }

        Ok(result)
    }

    /// Call an operator overloading method for the given prefix operator.
    fn create_prefix_operator_impl_call(
        &self,
        func: ast::Expression,
        rhs: ast::Expression,
        ret: Type,
        location: Location,
    ) -> Result<ast::Expression, MonomorphizationError> {
        let arguments = vec![rhs];
        let func = Box::new(func);
        let return_type = Self::convert_type(&ret, location)?;

        Ok(ast::Expression::Call(ast::Call { func, arguments, return_type, location }))
    }

    fn is_unconstrained(&self, func_id: node_interner::FuncId) -> bool {
        self.in_unconstrained_function
            || self.interner.function_modifiers(&func_id).is_unconstrained
    }
}

fn unwrap_tuple_type(typ: &HirType) -> Vec<HirType> {
    match typ.follow_bindings() {
        HirType::Tuple(fields) => fields.clone(),
        other => unreachable!("unwrap_tuple_type: expected tuple, found {:?}", other),
    }
}

fn unwrap_struct_type(
    typ: &HirType,
    location: Location,
) -> Result<Vec<(String, HirType, ItemVisibility)>, MonomorphizationError> {
    match typ.follow_bindings() {
        HirType::DataType(def, args) => {
            // Some of args might not be mentioned in fields, so we need to check that they aren't unbound.
            for arg in &args {
                Monomorphizer::check_type(arg, location)?;
            }

            Ok(def.borrow().get_fields(&args).unwrap())
        }
        other => unreachable!("unwrap_struct_type: expected struct, found {:?}", other),
    }
}

fn unwrap_enum_type(
    typ: &HirType,
    location: Location,
) -> Result<Vec<(String, Vec<HirType>)>, MonomorphizationError> {
    match typ.unwrap_forall().1.follow_bindings() {
        HirType::DataType(def, args) => {
            // Some of args might not be mentioned in fields, so we need to check that they aren't unbound.
            for arg in &args {
                Monomorphizer::check_type(arg, location)?;
            }

            Ok(def.borrow().get_variants(&args).unwrap())
        }
        other => unreachable!("unwrap_enum_type: expected enum, found {:?}", other),
    }
}

pub fn perform_instantiation_bindings(bindings: &TypeBindings) {
    for (var, _kind, binding) in bindings.values() {
        var.force_bind(binding.clone());
    }
}

pub fn undo_instantiation_bindings(bindings: TypeBindings) {
    for (id, (var, kind, _)) in bindings {
        var.unbind(id, kind);
    }
}

/// Call sites are instantiated against the trait method, but when an impl is later selected,
/// the corresponding method in the impl will have a different set of generics. `perform_impl_bindings`
/// is needed to apply the generics from the trait method to the impl method. Without this,
/// static method references to generic impls (e.g. `Eq::eq` for `[T; N]`) will fail to re-apply
/// the correct type bindings during monomorphization.
pub fn perform_impl_bindings(
    interner: &NodeInterner,
    trait_method: Option<TraitItemId>,
    impl_method: node_interner::FuncId,
    location: Location,
) -> Result<TypeBindings, InterpreterError> {
    let mut bindings = TypeBindings::default();

    if let Some(trait_method) = trait_method {
        let mut trait_method_type =
            interner.definition_type(trait_method.item_id).as_monotype().clone();

        let mut impl_method_type = interner.function_meta(&impl_method).typ.as_monotype().clone();

        // Make each NamedGeneric in this type bindable by replacing it with a TypeVariable
        // with the same internal id, binding.
        trait_method_type.replace_named_generics_with_type_variables();
        impl_method_type.replace_named_generics_with_type_variables();

        trait_method_type.try_unify(&impl_method_type, &mut bindings).map_err(|_| {
            InterpreterError::ImplMethodTypeMismatch {
                expected: trait_method_type.follow_bindings(),
                actual: impl_method_type.follow_bindings(),
                location,
            }
        })?;

        for (_, kind, binding) in bindings.values_mut() {
            *kind = kind.follow_bindings();
            *binding = binding.follow_bindings();
        }

        perform_instantiation_bindings(&bindings);
    }

    Ok(bindings)
}

/// Resolve a trait item to a particular impl
fn resolve_trait_item_impl(
    interner: &mut NodeInterner,
    method_id: TraitItemId,
    expr_id: ExprId,
) -> Result<node_interner::TraitImplId, InterpreterError> {
    let trait_impl = interner.get_selected_impl_for_expression(expr_id).ok_or_else(|| {
        let location = interner.expr_location(&expr_id);
        InterpreterError::NoImpl { location }
    })?;

    match trait_impl {
        TraitImplKind::Normal(impl_id) => Ok(impl_id),
        TraitImplKind::Assumed { object_type, trait_generics } => {
            let location = interner.expr_location(&expr_id);

            match interner.lookup_trait_implementation(
                &object_type,
                method_id.trait_id,
                &trait_generics.ordered,
                &trait_generics.named,
            ) {
                Ok((TraitImplKind::Normal(impl_id), instantiation_bindings)) => {
                    // Insert any additional instantiation bindings into this expression's instantiation bindings.
                    // This is similar to what's done in `verify_trait_constraint` in the frontend.
                    let mut bindings = interner.get_instantiation_bindings(expr_id).clone();
                    bindings.extend(instantiation_bindings);
                    interner.store_instantiation_bindings(expr_id, bindings);
                    Ok(impl_id)
                }
                Ok((TraitImplKind::Assumed { .. }, _instantiation_bindings)) => {
                    Err(InterpreterError::NoImpl { location })
                }
                Err(ImplSearchErrorKind::TypeAnnotationsNeededOnObjectType) => {
                    Err(InterpreterError::TypeAnnotationsNeededForMethodCall { location })
                }
                Err(ImplSearchErrorKind::Nested(constraints)) => {
                    if let Some(error) =
                        NoMatchingImplFoundError::new(interner, constraints, location)
                    {
                        Err(InterpreterError::NoMatchingImplFound { error })
                    } else {
                        Err(InterpreterError::NoImpl { location })
                    }
                }
                Err(ImplSearchErrorKind::MultipleMatching(candidates)) => {
                    Err(InterpreterError::MultipleMatchingImpls {
                        object_type,
                        location,
                        candidates,
                    })
                }
            }
        }
    }
}

pub(crate) fn resolve_trait_item(
    interner: &mut NodeInterner,
    method_id: TraitItemId,
    expr_id: ExprId,
) -> Result<TraitItem, InterpreterError> {
    let impl_id = resolve_trait_item_impl(interner, method_id, expr_id)?;

    let name = interner.definition_name(method_id.item_id);
    let impl_ = interner.get_trait_implementation(impl_id);
    let impl_ = impl_.borrow();

    for method in &impl_.methods {
        if interner.function_name(method) == name {
            return Ok(TraitItem::Method(*method));
        }
    }

    if let Some((id, expected_type)) = interner.get_trait_impl_associated_constant(impl_id, name) {
        // The lookup above returns the expected type but not the value that
        // is expected to resolve to a Type::Constant - we have to look that up separately.
        for item in interner.get_associated_types_for_impl(impl_id) {
            if item.name.as_str() == name {
                let id = *id;
                let expected_type = expected_type.clone();

                // We also need to apply any instantiation bindings if the expression has any
                let instantiation_bindings = interner.try_get_instantiation_bindings(expr_id);
                let value = if let Some(instantiation_bindings) = instantiation_bindings {
                    item.typ.substitute(instantiation_bindings)
                } else {
                    item.typ.clone()
                };

                return Ok(TraitItem::Constant { id, expected_type, value });
            }
        }
    }

    unreachable!("No method named `{name}` in impl")
}

pub(crate) enum TraitItem {
    Method(node_interner::FuncId),
    Constant { id: node_interner::DefinitionId, expected_type: Type, value: Type },
}

impl TraitItem {
    pub(crate) fn unwrap_method(&self) -> node_interner::FuncId {
        match self {
            TraitItem::Method(func_id) => *func_id,
            TraitItem::Constant { .. } => {
                panic!("Expected `TraitItem::Method`, but found `TraitItem::Constant`")
            }
        }
    }
}

/// Extend the arguments to `print` (which is a `bool` to show if newline is needed and
/// value to be printed itself) with a JSON serialized `PrintableType` to describe the
/// value, and another `bool` to show if the print is using a format string, or a raw
/// value.
pub fn append_printable_type_info_for_type(typ: Type, arguments: &mut Vec<ast::Expression>) {
    let typ: Type = typ.follow_bindings();
    let is_fmt_str = match typ {
        // A format string has many different possible types that need to be handled.
        // Loop over each element in the format string to fetch each type's relevant metadata
        Type::FmtString(_, elements) => {
            match *elements {
                Type::Tuple(element_types) => {
                    for typ in element_types {
                        append_printable_type_info_inner(&typ, arguments);
                    }
                }
                _ => unreachable!("ICE: format string type should be a tuple but got a {elements}"),
            }
            true
        }
        _ => {
            append_printable_type_info_inner(&typ, arguments);
            false
        }
    };
    // The caller needs information as to whether it is handling a format string or a single type
    arguments.push(ast::Expression::Literal(ast::Literal::Bool(is_fmt_str)));
}

fn append_printable_type_info_inner(typ: &Type, arguments: &mut Vec<ast::Expression>) {
    // Disallow printing slices and mutable references for consistency,
    // since they cannot be passed from ACIR into Brillig
    if matches!(typ, HirType::Reference(..)) {
        unreachable!("println and format strings do not support references.");
    }

    let printable_type: PrintableType = typ.into();
    let abi_as_string =
        serde_json::to_string(&printable_type).expect("ICE: expected PrintableType to serialize");

    arguments.push(ast::Expression::Literal(ast::Literal::Str(abi_as_string)));
}
