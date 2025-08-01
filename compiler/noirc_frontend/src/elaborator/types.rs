use std::{borrow::Cow, rc::Rc};

use im::HashSet;
use iter_extended::vecmap;
use noirc_errors::Location;
use rustc_hash::FxHashMap as HashMap;

use crate::{
    Generics, Kind, NamedGeneric, ResolvedGeneric, Type, TypeBinding, TypeBindings,
    UnificationError,
    ast::{
        AsTraitPath, BinaryOpKind, GenericTypeArgs, Ident, IntegerBitSize, PathKind, UnaryOp,
        UnresolvedGeneric, UnresolvedGenerics, UnresolvedType, UnresolvedTypeData,
        UnresolvedTypeExpression, WILDCARD_TYPE,
    },
    elaborator::UnstableFeature,
    hir::{
        def_collector::dc_crate::CompilationError,
        def_map::{ModuleDefId, fully_qualified_module_path},
        resolution::{errors::ResolverError, import::PathResolutionError},
        type_check::{
            NoMatchingImplFoundError, Source, TypeCheckError,
            generics::{Generic, TraitGenerics},
        },
    },
    hir_def::{
        expr::{
            HirBinaryOp, HirCallExpression, HirExpression, HirLiteral, HirMemberAccess,
            HirMethodReference, HirPrefixExpression, TraitItem,
        },
        function::FuncMeta,
        stmt::HirStatement,
        traits::{NamedType, ResolvedTraitBound, Trait, TraitConstraint},
    },
    modules::{get_ancestor_module_reexport, module_def_id_is_visible},
    node_interner::{
        DependencyId, ExprId, FuncId, GlobalValue, ImplSearchErrorKind, TraitId, TraitImplKind,
        TraitItemId,
    },
    shared::Signedness,
    token::SecondaryAttributeKind,
};

use super::{
    Elaborator, PathResolutionTarget, UnsafeBlockStatus, lints,
    path_resolution::{PathResolutionItem, PathResolutionMode, TypedPath},
};

pub const SELF_TYPE_NAME: &str = "Self";

pub(super) struct TraitPathResolution {
    pub(super) method: TraitPathResolutionMethod,
    pub(super) item: Option<PathResolutionItem>,
    pub(super) errors: Vec<PathResolutionError>,
}

pub(super) enum TraitPathResolutionMethod {
    NotATraitMethod(FuncId),
    TraitItem(TraitItem),
}

impl Elaborator<'_> {
    pub(crate) fn resolve_type(&mut self, typ: UnresolvedType, wildcard_allowed: bool) -> Type {
        self.resolve_type_inner(
            typ,
            &Kind::Normal,
            PathResolutionMode::MarkAsReferenced,
            wildcard_allowed,
        )
    }

    pub(crate) fn use_type(&mut self, typ: UnresolvedType, wildcard_allowed: bool) -> Type {
        self.use_type_with_kind(typ, &Kind::Normal, wildcard_allowed)
    }

    pub(crate) fn use_type_with_kind(
        &mut self,
        typ: UnresolvedType,
        kind: &Kind,
        wildcard_allowed: bool,
    ) -> Type {
        self.resolve_type_inner(typ, kind, PathResolutionMode::MarkAsUsed, wildcard_allowed)
    }

    /// Translates an UnresolvedType to a Type with a `TypeKind::Normal`
    fn resolve_type_inner(
        &mut self,
        typ: UnresolvedType,
        kind: &Kind,
        mode: PathResolutionMode,
        wildcard_allowed: bool,
    ) -> Type {
        let location = typ.location;
        let resolved_type = self.resolve_type_with_kind_inner(typ, kind, mode, wildcard_allowed);
        if resolved_type.is_nested_slice() {
            self.push_err(ResolverError::NestedSlices { location });
        }
        resolved_type
    }

    pub(crate) fn resolve_type_with_kind(
        &mut self,
        typ: UnresolvedType,
        kind: &Kind,
        wildcard_allowed: bool,
    ) -> Type {
        self.resolve_type_with_kind_inner(
            typ,
            kind,
            PathResolutionMode::MarkAsReferenced,
            wildcard_allowed,
        )
    }

    /// Translates an UnresolvedType into a Type and appends any
    /// freshly created TypeVariables created to new_variables.
    fn resolve_type_with_kind_inner(
        &mut self,
        typ: UnresolvedType,
        kind: &Kind,
        mode: PathResolutionMode,
        wildcard_allowed: bool,
    ) -> Type {
        use crate::ast::UnresolvedTypeData::*;

        let location = typ.location;
        let (named_path_location, is_self_type_name, is_synthetic) =
            if let Named(ref named_path, _, synthetic) = typ.typ {
                (
                    Some(named_path.last_ident().location()),
                    named_path.last_ident().is_self_type_name(),
                    synthetic,
                )
            } else {
                (None, false, false)
            };

        let resolved_type = match typ.typ {
            Array(size, elem) => {
                let elem = Box::new(self.resolve_type_with_kind_inner(
                    *elem,
                    kind,
                    mode,
                    wildcard_allowed,
                ));
                let size =
                    self.convert_expression_type(size, &Kind::u32(), location, wildcard_allowed);
                Type::Array(Box::new(size), elem)
            }
            Slice(elem) => {
                let elem = Box::new(self.resolve_type_with_kind_inner(
                    *elem,
                    kind,
                    mode,
                    wildcard_allowed,
                ));
                Type::Slice(elem)
            }
            Expression(expr) => {
                self.convert_expression_type(expr, kind, location, wildcard_allowed)
            }
            Unit => Type::Unit,
            Unspecified => {
                let location = typ.location;
                self.push_err(TypeCheckError::UnspecifiedType { location });
                Type::Error
            }
            Error => Type::Error,
            Named(path, args, _) => {
                let path = self.validate_path(path);
                self.resolve_named_type(path, args, mode, wildcard_allowed)
            }
            TraitAsType(path, args) => {
                let path = self.validate_path(path);
                self.resolve_trait_as_type(path, args, mode)
            }

            Tuple(fields) => Type::Tuple(vecmap(fields, |field| {
                self.resolve_type_with_kind_inner(field, kind, mode, wildcard_allowed)
            })),
            Function(args, ret, env, unconstrained) => {
                let args = vecmap(args, |arg| {
                    self.resolve_type_with_kind_inner(arg, kind, mode, wildcard_allowed)
                });
                let ret =
                    Box::new(self.resolve_type_with_kind_inner(*ret, kind, mode, wildcard_allowed));
                let env_location = env.location;

                let env =
                    Box::new(self.resolve_type_with_kind_inner(*env, kind, mode, wildcard_allowed));

                match *env {
                    Type::Unit | Type::Tuple(_) | Type::NamedGeneric(_) => {
                        Type::Function(args, ret, env, unconstrained)
                    }
                    _ => {
                        self.push_err(ResolverError::InvalidClosureEnvironment {
                            typ: *env,
                            location: env_location,
                        });
                        Type::Error
                    }
                }
            }
            Reference(element, mutable) => {
                if !mutable {
                    self.use_unstable_feature(UnstableFeature::Ownership, location);
                }
                Type::Reference(
                    Box::new(self.resolve_type_with_kind_inner(
                        *element,
                        kind,
                        mode,
                        wildcard_allowed,
                    )),
                    mutable,
                )
            }
            Parenthesized(typ) => {
                self.resolve_type_with_kind_inner(*typ, kind, mode, wildcard_allowed)
            }
            Resolved(id) => self.interner.get_quoted_type(id).clone(),
            AsTraitPath(path) => self.resolve_as_trait_path(*path, wildcard_allowed),
            Interned(id) => {
                let typ = self.interner.get_unresolved_type_data(id).clone();
                return self.resolve_type_with_kind_inner(
                    UnresolvedType { typ, location },
                    kind,
                    mode,
                    wildcard_allowed,
                );
            }
        };

        let location = named_path_location.unwrap_or(typ.location);
        match resolved_type {
            Type::DataType(ref data_type, _) => {
                // Record the location of the type reference
                self.interner.push_type_ref_location(&resolved_type, location);
                if !is_synthetic {
                    self.interner.add_type_reference(
                        data_type.borrow().id,
                        location,
                        is_self_type_name,
                    );
                }
            }
            Type::Alias(ref alias_type, _) => {
                self.interner.add_alias_reference(alias_type.borrow().id, location);
            }
            _ => (),
        }

        self.check_kind(resolved_type, kind, location)
    }

    pub fn find_generic(&self, target_name: &str) -> Option<&ResolvedGeneric> {
        self.generics.iter().find(|generic| generic.name.as_ref() == target_name)
    }

    // Resolve Self::Foo to an associated type on the current trait or trait impl
    fn lookup_associated_type_on_self(&self, path: &TypedPath) -> Option<Type> {
        if path.segments.len() == 2 && path.first_name() == Some(SELF_TYPE_NAME) {
            if let Some(trait_id) = self.current_trait {
                let the_trait = self.interner.get_trait(trait_id);
                if let Some(typ) = the_trait.get_associated_type(path.last_name()) {
                    return Some(typ.clone().as_named_generic());
                }
            }

            if let Some(impl_id) = self.current_trait_impl {
                let name = path.last_name();
                if let Some(typ) = self.interner.find_associated_type_for_impl(impl_id, name) {
                    return Some(typ.clone());
                }
            }
        }
        None
    }

    fn resolve_named_type(
        &mut self,
        path: TypedPath,
        args: GenericTypeArgs,
        mode: PathResolutionMode,
        wildcard_allowed: bool,
    ) -> Type {
        if args.is_empty() {
            if let Some(typ) = self.lookup_generic_or_global_type(&path, mode) {
                return typ;
            }
        }

        // Check if the path is a type variable first. We currently disallow generics on type
        // variables since we do not support higher-kinded types.
        if let Some(typ) = self.lookup_type_variable(&path, &args, wildcard_allowed) {
            return typ;
        }

        let location = path.location;

        if let Some(type_alias) = self.lookup_type_alias(path.clone(), mode) {
            let id = type_alias.borrow().id;
            let (args, _) =
                self.resolve_type_args_inner(args, id, location, mode, wildcard_allowed);

            if let Some(item) = self.current_item {
                self.interner.add_type_alias_dependency(item, id);
            }

            // Collecting Type Alias references [Location]s to be used by LSP in order
            // to resolve the definition of the type alias
            self.interner.add_type_alias_ref(id, location);

            // Because there is no ordering to when type aliases (and other globals) are resolved,
            // it is possible for one to refer to an Error type and issue no error if it is set
            // equal to another type alias. Fixing this fully requires an analysis to create a DFG
            // of definition ordering, but for now we have an explicit check here so that we at
            // least issue an error that the type was not found instead of silently passing.
            return Type::Alias(type_alias, args);
        }

        let location = path.location;
        match self.resolve_path_or_error_inner(path.clone(), PathResolutionTarget::Type, mode) {
            Ok(PathResolutionItem::Type(type_id)) => {
                let data_type = self.get_type(type_id);

                if self.resolving_ids.contains(&data_type.borrow().id) {
                    self.push_err(ResolverError::SelfReferentialType {
                        location: data_type.borrow().name.location(),
                    });

                    return Type::Error;
                }

                if !self.in_contract()
                    && self
                        .interner
                        .type_attributes(&data_type.borrow().id)
                        .iter()
                        .any(|attr| matches!(attr.kind, SecondaryAttributeKind::Abi(_)))
                {
                    self.push_err(ResolverError::AbiAttributeOutsideContract {
                        location: data_type.borrow().name.location(),
                    });
                }

                let (args, _) = self.resolve_type_args_inner(
                    args,
                    data_type.borrow(),
                    location,
                    mode,
                    wildcard_allowed,
                );

                if let Some(current_item) = self.current_item {
                    let dependency_id = data_type.borrow().id;
                    self.interner.add_type_dependency(current_item, dependency_id);
                }

                Type::DataType(data_type, args)
            }
            Ok(PathResolutionItem::PrimitiveType(primitive_type)) => {
                let typ = self.instantiate_primitive_type(
                    primitive_type,
                    args,
                    location,
                    wildcard_allowed,
                );
                if let Type::Quoted(quoted) = typ {
                    let in_function = matches!(self.current_item, Some(DependencyId::Function(_)));
                    if in_function && !self.in_comptime_context() {
                        let typ = quoted.to_string();
                        self.push_err(ResolverError::ComptimeTypeInRuntimeCode { location, typ });
                    }
                }
                typ
            }
            Ok(PathResolutionItem::TraitAssociatedType(associated_type_id)) => {
                let associated_type = self.interner.get_trait_associated_type(associated_type_id);
                let trait_ = self.interner.get_trait(associated_type.trait_id);

                self.push_err(ResolverError::AmbiguousAssociatedType {
                    trait_name: trait_.name.to_string(),
                    associated_type_name: associated_type.name.to_string(),
                    location,
                });

                Type::Error
            }
            Ok(item) => {
                self.push_err(ResolverError::Expected {
                    expected: "type",
                    got: item.description(),
                    location,
                });

                Type::Error
            }
            Err(err) => {
                self.push_err(err);

                Type::Error
            }
        }
    }

    fn lookup_type_variable(
        &mut self,
        path: &TypedPath,
        args: &GenericTypeArgs,
        wildcard_allowed: bool,
    ) -> Option<Type> {
        if path.segments.len() != 1 {
            return None;
        }

        let name = path.last_name();
        match name {
            SELF_TYPE_NAME => {
                let self_type = self.self_type.clone()?;
                if !args.is_empty() {
                    self.push_err(ResolverError::GenericsOnSelfType { location: path.location });
                }
                Some(self_type)
            }
            WILDCARD_TYPE => {
                if !wildcard_allowed {
                    self.push_err(ResolverError::WildcardTypeDisallowed {
                        location: path.location,
                    });
                }

                Some(self.interner.next_type_variable_with_kind(Kind::Any))
            }
            _ => None,
        }
    }

    fn resolve_trait_as_type(
        &mut self,
        path: TypedPath,
        args: GenericTypeArgs,
        mode: PathResolutionMode,
    ) -> Type {
        // Fetch information needed from the trait as the closure for resolving all the `args`
        // requires exclusive access to `self`
        let location = path.location;
        let trait_as_type_info = self.lookup_trait_or_error(path).map(|t| t.id);

        if let Some(id) = trait_as_type_info {
            let wildcard_allowed = false;
            let (ordered, named) =
                self.resolve_type_args_inner(args, id, location, mode, wildcard_allowed);
            let name = self.interner.get_trait(id).name.to_string();
            let generics = TraitGenerics { ordered, named };
            Type::TraitAsType(id, Rc::new(name), generics)
        } else {
            Type::Error
        }
    }

    /// Identical to `resolve_type_args` but does not allow
    /// associated types to be elided since trait impls must specify them.
    pub(super) fn resolve_trait_args_from_trait_impl(
        &mut self,
        args: GenericTypeArgs,
        item: TraitId,
        location: Location,
    ) -> (Vec<Type>, Vec<NamedType>) {
        let mode = PathResolutionMode::MarkAsReferenced;
        let allow_implicit_named_args = false;
        let wildcard_allowed = true;
        self.resolve_type_or_trait_args_inner(
            args,
            item,
            location,
            allow_implicit_named_args,
            mode,
            wildcard_allowed,
        )
    }

    pub(super) fn use_type_args(
        &mut self,
        args: GenericTypeArgs,
        item: impl Generic,
        location: Location,
    ) -> (Vec<Type>, Vec<NamedType>) {
        let mode = PathResolutionMode::MarkAsUsed;
        let wildcard_allowed = true;
        self.resolve_type_args_inner(args, item, location, mode, wildcard_allowed)
    }

    pub(super) fn resolve_type_args_inner(
        &mut self,
        args: GenericTypeArgs,
        item: impl Generic,
        location: Location,
        mode: PathResolutionMode,
        wildcard_allowed: bool,
    ) -> (Vec<Type>, Vec<NamedType>) {
        let allow_implicit_named_args = true;
        self.resolve_type_or_trait_args_inner(
            args,
            item,
            location,
            allow_implicit_named_args,
            mode,
            wildcard_allowed,
        )
    }

    pub(super) fn resolve_type_or_trait_args_inner(
        &mut self,
        mut args: GenericTypeArgs,
        item: impl Generic,
        location: Location,
        allow_implicit_named_args: bool,
        mode: PathResolutionMode,
        wildcard_allowed: bool,
    ) -> (Vec<Type>, Vec<NamedType>) {
        let expected_kinds = item.generic_kinds(self.interner);

        if args.ordered_args.len() != expected_kinds.len() {
            self.push_err(TypeCheckError::GenericCountMismatch {
                item: item.item_name(self.interner),
                expected: expected_kinds.len(),
                found: args.ordered_args.len(),
                location,
            });
            let error_type = UnresolvedTypeData::Error.with_location(location);
            args.ordered_args.resize(expected_kinds.len(), error_type);
        }

        let ordered_args = expected_kinds.iter().zip(args.ordered_args);
        let ordered = vecmap(ordered_args, |(kind, typ)| {
            self.resolve_type_with_kind_inner(typ, kind, mode, wildcard_allowed)
        });

        let mut associated = Vec::new();

        if item.accepts_named_type_args() {
            associated = self.resolve_associated_type_args(
                args.named_args,
                item,
                location,
                allow_implicit_named_args,
                mode,
                wildcard_allowed,
            );
        } else if !args.named_args.is_empty() {
            let item_kind = item.item_kind();
            self.push_err(ResolverError::NamedTypeArgs { location, item_kind });
        }

        (ordered, associated)
    }

    fn resolve_associated_type_args(
        &mut self,
        args: Vec<(Ident, UnresolvedType)>,
        item: impl Generic,
        location: Location,
        allow_implicit_named_args: bool,
        mode: PathResolutionMode,
        wildcard_allowed: bool,
    ) -> Vec<NamedType> {
        let mut seen_args = HashMap::default();
        let mut required_args = item.named_generics(self.interner);
        let mut resolved = Vec::with_capacity(required_args.len());

        // Go through each argument to check if it is in our required_args list.
        // If it is remove it from the list, otherwise issue an error.
        for (name, typ) in args {
            let index = required_args.iter().position(|item| item.name.as_ref() == name.as_str());

            let Some(index) = index else {
                if let Some(prev_location) = seen_args.get(name.as_str()).copied() {
                    self.push_err(TypeCheckError::DuplicateNamedTypeArg { name, prev_location });
                } else {
                    let item = item.item_name(self.interner);
                    self.push_err(TypeCheckError::NoSuchNamedTypeArg { name, item });
                }
                continue;
            };

            // Remove the argument from the required list so we remember that we already have it
            let expected = required_args.remove(index);
            seen_args.insert(name.to_string(), name.location());

            let typ =
                self.resolve_type_with_kind_inner(typ, &expected.kind(), mode, wildcard_allowed);
            resolved.push(NamedType { name, typ });
        }

        // Anything that hasn't been removed yet is missing.
        // Fill it in to avoid a panic if we allow named args to be elided, otherwise error.
        for generic in required_args {
            let name = generic.name.clone();

            if allow_implicit_named_args {
                let name = Ident::new(name.as_ref().clone(), location);
                let typ = self.interner.next_type_variable();
                resolved.push(NamedType { name, typ });
            } else {
                let item = item.item_name(self.interner);
                self.push_err(TypeCheckError::MissingNamedTypeArg { item, location, name });
            }
        }

        resolved
    }

    fn lookup_generic_or_global_type(
        &mut self,
        path: &TypedPath,
        mode: PathResolutionMode,
    ) -> Option<Type> {
        if path.segments.len() == 1 {
            let name = path.last_name();
            if let Some(generic) = self.find_generic(name) {
                let generic = generic.clone();
                return Some(generic.as_named_generic());
            }
        } else if let Some(typ) = self.lookup_associated_type_on_self(path) {
            if let Some(last_segment) = path.segments.last() {
                if last_segment.generics.is_some() {
                    self.push_err(ResolverError::GenericsOnAssociatedType {
                        location: last_segment.turbofish_location(),
                    });
                }
            }
            return Some(typ);
        }

        // If we cannot find a local generic of the same name, try to look up a global
        match self.resolve_path_or_error_inner(path.clone(), PathResolutionTarget::Value, mode) {
            Ok(PathResolutionItem::Global(id)) => {
                if let Some(current_item) = self.current_item {
                    self.interner.add_global_dependency(current_item, id);
                }

                let reference_location = path.location;
                self.interner.add_global_reference(id, reference_location);
                let kind = self
                    .interner
                    .get_global_let_statement(id)
                    .map(|let_statement| Kind::numeric(let_statement.r#type))
                    .unwrap_or(Kind::u32());

                let Some(stmt) = self.interner.get_global_let_statement(id) else {
                    if self.elaborate_global_if_unresolved(&id) {
                        return self.lookup_generic_or_global_type(path, mode);
                    } else {
                        let path = path.clone();
                        self.push_err(ResolverError::NoSuchNumericTypeVariable { path });
                        return None;
                    }
                };

                let rhs = stmt.expression;
                let location = self.interner.expr_location(&rhs);

                let GlobalValue::Resolved(global_value) = &self.interner.get_global(id).value
                else {
                    self.push_err(ResolverError::UnevaluatedGlobalType { location });
                    return None;
                };

                let Some(global_value) = global_value.to_field_element() else {
                    let global_value = global_value.clone();
                    if global_value.is_integral() {
                        self.push_err(ResolverError::NegativeGlobalType { location, global_value });
                    } else {
                        self.push_err(ResolverError::NonIntegralGlobalType {
                            location,
                            global_value,
                        });
                    }
                    return None;
                };

                let Ok(global_value) = kind.ensure_value_fits(global_value, location) else {
                    self.push_err(ResolverError::GlobalLargerThanKind {
                        location,
                        global_value,
                        kind,
                    });
                    return None;
                };

                Some(Type::Constant(global_value, kind))
            }
            _ => None,
        }
    }

    pub(super) fn convert_expression_type(
        &mut self,
        length: UnresolvedTypeExpression,
        expected_kind: &Kind,
        location: Location,
        wildcard_allowed: bool,
    ) -> Type {
        match length {
            UnresolvedTypeExpression::Variable(path) => {
                let path = self.validate_path(path);
                let mode = PathResolutionMode::MarkAsReferenced;
                let typ = self.resolve_named_type(
                    path,
                    GenericTypeArgs::default(),
                    mode,
                    wildcard_allowed,
                );
                self.check_kind(typ, expected_kind, location)
            }
            UnresolvedTypeExpression::Constant(int, suffix, _span) => {
                if let Some(suffix) = suffix {
                    self.check_kind(suffix.as_type(), expected_kind, location);
                }
                Type::Constant(int, expected_kind.clone())
            }
            UnresolvedTypeExpression::BinaryOperation(lhs, op, rhs, location) => {
                let (lhs_location, rhs_location) = (lhs.location(), rhs.location());
                let lhs = self.convert_expression_type(
                    *lhs,
                    expected_kind,
                    lhs_location,
                    wildcard_allowed,
                );
                let rhs = self.convert_expression_type(
                    *rhs,
                    expected_kind,
                    rhs_location,
                    wildcard_allowed,
                );

                match (lhs, rhs) {
                    (Type::Constant(lhs, lhs_kind), Type::Constant(rhs, rhs_kind)) => {
                        if !lhs_kind.unifies(&rhs_kind) {
                            self.push_err(TypeCheckError::TypeKindMismatch {
                                expected_kind: lhs_kind,
                                expr_kind: rhs_kind,
                                expr_location: location,
                            });
                            return Type::Error;
                        }
                        match op.function(lhs, rhs, &lhs_kind, location) {
                            Ok(result) => Type::Constant(result, lhs_kind),
                            Err(err) => {
                                let err = Box::new(err);
                                let error =
                                    ResolverError::BinaryOpError { lhs, op, rhs, err, location };
                                self.push_err(error);
                                Type::Error
                            }
                        }
                    }
                    (lhs, rhs) => {
                        let infix = Type::infix_expr(Box::new(lhs), op, Box::new(rhs));
                        Type::CheckedCast { from: Box::new(infix.clone()), to: Box::new(infix) }
                            .canonicalize()
                    }
                }
            }
            UnresolvedTypeExpression::AsTraitPath(path) => {
                let typ = self.resolve_as_trait_path(*path, wildcard_allowed);
                self.check_kind(typ, expected_kind, location)
            }
        }
    }

    pub(super) fn check_kind(
        &mut self,
        typ: Type,
        expected_kind: &Kind,
        location: Location,
    ) -> Type {
        if !typ.kind().unifies(expected_kind) {
            self.push_err(TypeCheckError::TypeKindMismatch {
                expected_kind: expected_kind.clone(),
                expr_kind: typ.kind(),
                expr_location: location,
            });
            return Type::Error;
        }
        typ
    }

    fn resolve_as_trait_path(&mut self, path: AsTraitPath, wildcard_allowed: bool) -> Type {
        let location = path.trait_path.location;
        let trait_path = self.validate_path(path.trait_path.clone());
        let Some(trait_id) = self.resolve_trait_by_path(trait_path) else {
            // Error should already be pushed in the None case
            return Type::Error;
        };

        let (ordered, named) = self.use_type_args(path.trait_generics.clone(), trait_id, location);
        let object_type = self.use_type(path.typ.clone(), wildcard_allowed);

        match self.interner.lookup_trait_implementation(&object_type, trait_id, &ordered, &named) {
            Ok((impl_kind, instantiation_bindings)) => {
                let typ = self.get_associated_type_from_trait_impl(path, impl_kind);
                typ.substitute(&instantiation_bindings)
            }
            Err(constraints) => {
                self.push_trait_constraint_error(&object_type, constraints, location);
                Type::Error
            }
        }
    }

    fn get_associated_type_from_trait_impl(
        &mut self,
        path: AsTraitPath,
        impl_kind: TraitImplKind,
    ) -> Type {
        let associated_types = match impl_kind {
            TraitImplKind::Assumed { trait_generics, .. } => Cow::Owned(trait_generics.named),
            TraitImplKind::Normal(impl_id) => {
                Cow::Borrowed(self.interner.get_associated_types_for_impl(impl_id))
            }
        };

        match associated_types.iter().find(|named| named.name == path.impl_item) {
            Some(generic) => generic.typ.clone(),
            None => {
                let name = path.impl_item.clone();
                let item = format!("<{} as {}>", path.typ, path.trait_path);
                self.push_err(TypeCheckError::NoSuchNamedTypeArg { name, item });
                Type::Error
            }
        }
    }

    // this resolves Self::some_static_method, inside an impl block (where we don't have a concrete self_type)
    // or inside a trait default method.
    //
    // Returns the trait method, trait constraint, and whether the impl is assumed to exist by a where clause or not
    // E.g. `t.method()` with `where T: Foo<Bar>` in scope will return `(Foo::method, T, vec![Bar])`
    fn resolve_trait_static_method_by_self(
        &mut self,
        path: &TypedPath,
    ) -> Option<TraitPathResolution> {
        // If we are inside a trait impl, `Self` is known to be a concrete type so we don't have
        // to solve the path via trait method lookup.
        if self.current_trait_impl.is_some() {
            return None;
        }

        let trait_id = self.current_trait?;

        if path.kind == PathKind::Plain && path.segments.len() == 2 {
            let name = path.segments[0].ident.as_str();
            let method = &path.segments[1].ident;

            if name == SELF_TYPE_NAME {
                let the_trait = self.interner.get_trait(trait_id);
                // Allow referring to trait constants via Self:: as well
                let definition =
                    the_trait.find_method_or_constant(method.as_str(), self.interner)?;
                let constraint = the_trait.as_constraint(path.location);
                let trait_method = TraitItem { definition, constraint, assumed: true };
                let method = TraitPathResolutionMethod::TraitItem(trait_method);
                return Some(TraitPathResolution { method, item: None, errors: Vec::new() });
            }
        }
        None
    }

    // this resolves TraitName::some_static_method
    //
    // Returns the trait method, trait constraint, and whether the impl is assumed to exist by a where clause or not
    // E.g. `t.method()` with `where T: Foo<Bar>` in scope will return `(Foo::method, T, vec![Bar])`
    fn resolve_trait_static_method(&mut self, path: &TypedPath) -> Option<TraitPathResolution> {
        let path_resolution = self.use_path_as_type(path.clone()).ok()?;
        let func_id = path_resolution.item.function_id()?;
        let meta = self.interner.try_function_meta(&func_id)?;
        let the_trait = self.interner.get_trait(meta.trait_id?);
        let method = the_trait.find_method(path.last_name(), self.interner)?;
        let constraint = the_trait.as_constraint(path.location);
        let trait_method = TraitItem { definition: method, constraint, assumed: false };
        let method = TraitPathResolutionMethod::TraitItem(trait_method);
        let item = Some(path_resolution.item);
        Some(TraitPathResolution { method, item, errors: path_resolution.errors })
    }

    // This resolves a static trait method T::trait_method by iterating over the where clause
    //
    // Returns the trait method, trait constraint, and whether the impl is assumed from a where
    // clause. This is always true since this helper searches where clauses for a generic constraint.
    // E.g. `t.method()` with `where T: Foo<Bar>` in scope will return `(Foo::method, T, vec![Bar])`
    fn resolve_trait_method_by_named_generic(
        &mut self,
        path: &TypedPath,
    ) -> Option<TraitPathResolution> {
        if path.segments.len() != 2 {
            return None;
        }

        for constraint in self.trait_bounds.clone() {
            if let Type::NamedGeneric(NamedGeneric { name, .. }) = &constraint.typ {
                // if `path` is `T::method_name`, we're looking for constraint of the form `T: SomeTrait`
                if path.segments[0].ident.as_str() != name.as_str() {
                    continue;
                }

                let the_trait = self.interner.get_trait(constraint.trait_bound.trait_id);
                if let Some(definition) =
                    the_trait.find_method_or_constant(path.last_name(), self.interner)
                {
                    let trait_item = TraitItem { definition, constraint, assumed: true };
                    let method = TraitPathResolutionMethod::TraitItem(trait_item);
                    return Some(TraitPathResolution { method, item: None, errors: Vec::new() });
                }
            }
        }
        None
    }

    /// This resolves a method in the form `Type::method` where `method` is a trait method
    fn resolve_type_trait_method(&mut self, path: &TypedPath) -> Option<TraitPathResolution> {
        if path.segments.len() < 2 {
            return None;
        }

        let mut path = path.clone();
        let location = path.location;
        let last_segment = path.pop();
        let before_last_segment = path.last_segment();
        let turbofish = before_last_segment.turbofish();

        let path_resolution = self.use_path_as_type(path).ok()?;
        let typ = match path_resolution.item {
            PathResolutionItem::Type(type_id) => {
                let generics = self.resolve_struct_id_turbofish_generics(type_id, turbofish);
                let datatype = self.get_type(type_id);
                Type::DataType(datatype, generics)
            }
            PathResolutionItem::TypeAlias(type_alias_id) => {
                let generics =
                    self.resolve_type_alias_id_turbofish_generics(type_alias_id, turbofish);
                let type_alias = self.interner.get_type_alias(type_alias_id);
                let type_alias = type_alias.borrow();
                type_alias.get_type(&generics)
            }
            PathResolutionItem::PrimitiveType(primitive_type) => {
                self.instantiate_primitive_type_with_turbofish(primitive_type, turbofish)
            }
            PathResolutionItem::Module(..)
            | PathResolutionItem::Trait(..)
            | PathResolutionItem::TraitAssociatedType(..)
            | PathResolutionItem::Global(..)
            | PathResolutionItem::ModuleFunction(..)
            | PathResolutionItem::Method(..)
            | PathResolutionItem::SelfMethod(..)
            | PathResolutionItem::TypeAliasFunction(..)
            | PathResolutionItem::TraitFunction(..)
            | PathResolutionItem::TypeTraitFunction(..)
            | PathResolutionItem::PrimitiveFunction(..) => {
                return None;
            }
        };

        let method_name = last_segment.ident.as_str();

        // If we can find a method on the type, this is definitely not a trait method
        if self.interner.lookup_direct_method(&typ, method_name, false).is_some() {
            return None;
        }

        let trait_methods = self.interner.lookup_trait_methods(&typ, method_name, false);

        if trait_methods.is_empty() {
            return None;
        }

        let (hir_method_reference, error) =
            self.get_trait_method_in_scope(&trait_methods, method_name, last_segment.location);
        let hir_method_reference = hir_method_reference?;
        let func_id = hir_method_reference.func_id(self.interner)?;
        match hir_method_reference {
            HirMethodReference::FuncId(func_id) => {
                // It could happen that we find a single function (one in a trait impl)
                let mut errors = path_resolution.errors;
                if let Some(error) = error {
                    errors.push(error);
                }

                let method = TraitPathResolutionMethod::NotATraitMethod(func_id);
                Some(TraitPathResolution { method, item: None, errors })
            }
            HirMethodReference::TraitItemId(definition, trait_id, _, _) => {
                let trait_ = self.interner.get_trait(trait_id);
                let mut constraint = trait_.as_constraint(location);
                constraint.typ = typ.clone();

                let trait_method = TraitItem { definition, constraint, assumed: false };
                let item = PathResolutionItem::TypeTraitFunction(typ, trait_id, func_id);

                let mut errors = path_resolution.errors;
                if let Some(error) = error {
                    errors.push(error);
                }

                let method = TraitPathResolutionMethod::TraitItem(trait_method);
                Some(TraitPathResolution { method, item: Some(item), errors })
            }
        }
    }

    // Try to resolve the given trait method path.
    //
    // Returns the trait method, trait constraint, and whether the impl is assumed to exist by a where clause or not
    // E.g. `t.method()` with `where T: Foo<Bar>` in scope will return `(Foo::method, T, vec![Bar])`
    pub(super) fn resolve_trait_generic_path(
        &mut self,
        path: &TypedPath,
    ) -> Option<TraitPathResolution> {
        self.resolve_trait_static_method_by_self(path)
            .or_else(|| self.resolve_trait_static_method(path))
            .or_else(|| self.resolve_trait_method_by_named_generic(path))
            .or_else(|| self.resolve_type_trait_method(path))
    }

    pub(super) fn unify(
        &mut self,
        actual: &Type,
        expected: &Type,
        make_error: impl FnOnce() -> TypeCheckError,
    ) {
        if let Err(UnificationError) = actual.unify(expected) {
            self.push_err(make_error());
        }
    }

    /// Do not apply type bindings even after a successful unification.
    /// This function is used by the interpreter for some comptime code
    /// which can change types e.g. on each iteration of a for loop.
    pub fn unify_without_applying_bindings(
        &mut self,
        actual: &Type,
        expected: &Type,
        make_error: impl FnOnce() -> TypeCheckError,
    ) {
        let mut bindings = TypeBindings::default();
        if actual.try_unify(expected, &mut bindings).is_err() {
            let error: CompilationError = make_error().into();
            self.push_err(error);
        }
    }

    /// Wrapper of Type::unify_with_coercions using self.errors
    pub(super) fn unify_with_coercions(
        &mut self,
        actual: &Type,
        expected: &Type,
        expression: ExprId,
        location: Location,
        make_error: impl FnOnce() -> CompilationError,
    ) {
        let mut errors = Vec::new();
        actual.unify_with_coercions(
            expected,
            expression,
            location,
            self.interner,
            &mut errors,
            make_error,
        );
        self.push_errors(errors);
    }

    /// Return a fresh integer or field type variable and log it
    /// in self.type_variables to default it later.
    pub(super) fn polymorphic_integer_or_field(&mut self) -> Type {
        let typ = Type::polymorphic_integer_or_field(self.interner);
        self.push_defaultable_type_variable(typ.clone());
        typ
    }

    /// Return a fresh integer type variable and log it
    /// in self.type_variables to default it later.
    pub(super) fn polymorphic_integer(&mut self) -> Type {
        let typ = Type::polymorphic_integer(self.interner);
        self.push_defaultable_type_variable(typ.clone());
        typ
    }

    /// Return a fresh integer type variable and log it
    /// in self.type_variables to default it later.
    pub(super) fn type_variable_with_kind(&mut self, type_var_kind: Kind) -> Type {
        let typ = Type::type_variable_with_kind(self.interner, type_var_kind);
        self.push_defaultable_type_variable(typ.clone());
        typ
    }

    /// Translates a (possibly Unspecified) UnresolvedType to a Type.
    /// Any UnresolvedType::Unspecified encountered are replaced with fresh type variables.
    pub(super) fn resolve_inferred_type(&mut self, typ: UnresolvedType) -> Type {
        match &typ.typ {
            UnresolvedTypeData::Unspecified => {
                self.interner.next_type_variable_with_kind(Kind::Any)
            }
            _ => {
                let wildcard_allowed = true;
                self.use_type(typ, wildcard_allowed)
            }
        }
    }

    /// Insert as many dereference operations as necessary to automatically dereference a method
    /// call object to its base value type T.
    pub(super) fn insert_auto_dereferences(&mut self, object: ExprId, typ: Type) -> (ExprId, Type) {
        if let Type::Reference(element, _mut) = typ.follow_bindings() {
            let location = self.interner.id_location(object);

            let object = self.interner.push_expr(HirExpression::Prefix(HirPrefixExpression::new(
                UnaryOp::Dereference { implicitly_added: true },
                object,
            )));
            self.interner.push_expr_type(object, element.as_ref().clone());
            self.interner.push_expr_location(object, location);

            // Recursively dereference to allow for converting &mut &mut T to T
            self.insert_auto_dereferences(object, *element)
        } else {
            (object, typ)
        }
    }

    /// Given a method object: `(*foo).bar` of a method call `(*foo).bar.baz()`, remove the
    /// implicitly added dereference operator if one is found.
    ///
    /// Returns Some(new_expr_id) if a dereference was removed and None otherwise.
    fn try_remove_implicit_dereference(&mut self, object: ExprId) -> Option<ExprId> {
        match self.interner.expression(&object) {
            HirExpression::MemberAccess(mut access) => {
                let new_lhs = self.try_remove_implicit_dereference(access.lhs)?;
                access.lhs = new_lhs;
                access.is_offset = true;

                // `object` will have a different type now, which will be filled in
                // later when type checking the method call as a function call.
                self.interner.replace_expr(&object, HirExpression::MemberAccess(access));
                Some(object)
            }
            HirExpression::Prefix(prefix) => match prefix.operator {
                // Found a dereference we can remove. Now just replace it with its rhs to remove it.
                UnaryOp::Dereference { implicitly_added: true } => Some(prefix.rhs),
                _ => None,
            },
            _ => None,
        }
    }

    fn bind_function_type_impl(
        &mut self,
        fn_params: &[Type],
        fn_ret: &Type,
        callsite_args: &[(Type, ExprId, Location)],
        location: Location,
    ) -> Type {
        if fn_params.len() != callsite_args.len() {
            self.push_err(TypeCheckError::ParameterCountMismatch {
                expected: fn_params.len(),
                found: callsite_args.len(),
                location,
            });
            return Type::Error;
        }

        for (param, (arg, arg_expr_id, arg_location)) in fn_params.iter().zip(callsite_args) {
            self.unify_with_coercions(arg, param, *arg_expr_id, *arg_location, || {
                CompilationError::TypeError(TypeCheckError::TypeMismatch {
                    expected_typ: param.to_string(),
                    expr_typ: arg.to_string(),
                    expr_location: *arg_location,
                })
            });
        }

        fn_ret.clone()
    }

    pub(super) fn bind_function_type(
        &mut self,
        function: Type,
        args: Vec<(Type, ExprId, Location)>,
        location: Location,
    ) -> Type {
        // Could do a single unification for the entire function type, but matching beforehand
        // lets us issue a more precise error on the individual argument that fails to type check.
        match function {
            Type::TypeVariable(binding) if binding.kind() == Kind::Normal => {
                if let TypeBinding::Bound(typ) = &*binding.borrow() {
                    return self.bind_function_type(typ.clone(), args, location);
                }

                let ret = self.interner.next_type_variable();
                let args = vecmap(args, |(arg, _, _)| arg);
                let env_type = self.interner.next_type_variable();
                let expected =
                    Type::Function(args, Box::new(ret.clone()), Box::new(env_type), false);

                let expected_kind = expected.kind();
                if let Err(error) = binding.try_bind(expected, &expected_kind, location) {
                    self.push_err(error);
                }
                ret
            }
            // The closure env is ignored on purpose: call arguments never place
            // constraints on closure environments.
            Type::Function(parameters, ret, _env, _unconstrained) => {
                self.bind_function_type_impl(&parameters, &ret, &args, location)
            }
            Type::Error => Type::Error,
            found => {
                self.push_err(TypeCheckError::ExpectedFunction { found, location });
                Type::Error
            }
        }
    }

    pub(super) fn check_cast(
        &mut self,
        from_expr_id: &ExprId,
        from: &Type,
        to: &Type,
        location: Location,
    ) -> Type {
        let to = to.follow_bindings();
        let from_follow_bindings = from.follow_bindings();

        use HirExpression::Literal;
        let from_value_opt = match self.interner.expression(from_expr_id) {
            Literal(HirLiteral::Integer(field)) if !field.is_negative() => {
                Some(field.absolute_value())
            }

            // TODO(https://github.com/noir-lang/noir/issues/6247):
            // handle negative literals
            _ => None,
        };

        let from_is_polymorphic = match from_follow_bindings {
            Type::Integer(..) | Type::FieldElement | Type::Bool => false,

            Type::TypeVariable(ref var) if var.is_integer() || var.is_integer_or_field() => true,
            Type::TypeVariable(_) => {
                // NOTE: in reality the expected type can also include bool, but for the compiler's simplicity
                // we only allow integer types. If a bool is in `from` it will need an explicit type annotation.
                let expected = self.polymorphic_integer_or_field();
                self.unify(from, &expected, || TypeCheckError::InvalidCast {
                    from: from.clone(),
                    location,
                    reason: "casting from a non-integral type is unsupported".into(),
                });
                true
            }
            Type::Error => return Type::Error,
            from => {
                let reason = "casting from this type is unsupported".into();
                self.push_err(TypeCheckError::InvalidCast { from, location, reason });
                return Type::Error;
            }
        };

        // TODO(https://github.com/noir-lang/noir/issues/6247):
        // handle negative literals
        // when casting a polymorphic value to a specifically sized type,
        // check that it fits or throw a warning
        if let (Some(from_value), Some(to_maximum_size)) =
            (from_value_opt, to.integral_maximum_size())
        {
            if from_is_polymorphic && from_value > to_maximum_size {
                let from = from.clone();
                let to = to.clone();
                let reason = format!(
                    "casting untyped value ({from_value}) to a type with a maximum size ({to_maximum_size}) that's smaller than it"
                );
                // we warn that the 'to' type is too small for the value
                self.push_err(TypeCheckError::DownsizingCast { from, to, location, reason });
            }
        }

        match to {
            Type::Integer(sign, bits) => Type::Integer(sign, bits),
            Type::FieldElement => {
                if from_follow_bindings.is_signed() {
                    self.push_err(TypeCheckError::UnsupportedFieldCast { location });
                }

                Type::FieldElement
            }
            Type::Bool => {
                let from_is_numeric = match from_follow_bindings {
                    Type::Integer(..) | Type::FieldElement => true,
                    Type::TypeVariable(ref var) => var.is_integer() || var.is_integer_or_field(),
                    _ => false,
                };
                if from_is_numeric {
                    self.push_err(TypeCheckError::CannotCastNumericToBool {
                        typ: from_follow_bindings,
                        location,
                    });
                }

                Type::Bool
            }
            Type::Error => Type::Error,
            _ => {
                self.push_err(TypeCheckError::UnsupportedCast { location });
                Type::Error
            }
        }
    }

    // Given a binary comparison operator and another type. This method will produce the output type
    // and a boolean indicating whether to use the trait impl corresponding to the operator
    // or not. A value of false indicates the caller to use a primitive operation for this
    // operator, while a true value indicates a user-provided trait impl is required.
    fn comparator_operand_type_rules(
        &mut self,
        lhs_type: &Type,
        rhs_type: &Type,
        op: &HirBinaryOp,
        location: Location,
    ) -> Result<(Type, bool), TypeCheckError> {
        use Type::*;

        match (lhs_type, rhs_type) {
            // Avoid reporting errors multiple times
            (Error, _) | (_, Error) => Ok((Bool, false)),
            (Alias(alias, args), other) | (other, Alias(alias, args)) => {
                let alias = alias.borrow().get_type(args);
                self.comparator_operand_type_rules(&alias, other, op, location)
            }

            // Matches on TypeVariable must be first to follow any type
            // bindings.
            (TypeVariable(var), other) | (other, TypeVariable(var)) => {
                if let TypeBinding::Bound(binding) = &*var.borrow() {
                    return self.comparator_operand_type_rules(other, binding, op, location);
                }

                let use_impl = self.bind_type_variables_for_infix(lhs_type, op, rhs_type, location);
                Ok((Bool, use_impl))
            }
            (Integer(sign_x, bit_width_x), Integer(sign_y, bit_width_y)) => {
                if sign_x != sign_y {
                    return Err(TypeCheckError::IntegerSignedness {
                        sign_x: *sign_x,
                        sign_y: *sign_y,
                        location,
                    });
                }
                if bit_width_x != bit_width_y {
                    return Err(TypeCheckError::IntegerBitWidth {
                        bit_width_x: *bit_width_x,
                        bit_width_y: *bit_width_y,
                        location,
                    });
                }
                Ok((Bool, false))
            }
            (FieldElement, FieldElement) => {
                if op.kind.is_valid_for_field_type() {
                    Ok((Bool, false))
                } else {
                    Err(TypeCheckError::FieldComparison { location })
                }
            }

            // <= and friends are technically valid for booleans, just not very useful
            (Bool, Bool) => Ok((Bool, false)),

            (lhs, rhs) => {
                self.unify(lhs, rhs, || TypeCheckError::TypeMismatchWithSource {
                    expected: lhs.clone(),
                    actual: rhs.clone(),
                    location: op.location,
                    source: Source::Binary,
                });
                Ok((Bool, true))
            }
        }
    }

    /// Handles the TypeVariable case for checking binary operators.
    /// Returns true if we should use the impl for the operator instead of the primitive
    /// version of it.
    fn bind_type_variables_for_infix(
        &mut self,
        lhs_type: &Type,
        op: &HirBinaryOp,
        rhs_type: &Type,
        location: Location,
    ) -> bool {
        self.unify(lhs_type, rhs_type, || TypeCheckError::TypeMismatchWithSource {
            expected: lhs_type.clone(),
            actual: rhs_type.clone(),
            source: Source::Binary,
            location,
        });

        let use_impl = !lhs_type.is_numeric_value();

        // If this operator isn't valid for fields we have to possibly narrow
        // Kind::IntegerOrField to Kind::Integer.
        // Doing so also ensures a type error if Field is used.
        // The is_numeric check is to allow impls for custom types to bypass this.
        if !op.kind.is_valid_for_field_type() && lhs_type.is_numeric_value() {
            let target = self.polymorphic_integer();

            use crate::ast::BinaryOpKind::*;
            use TypeCheckError::*;
            self.unify(lhs_type, &target, || match op.kind {
                Less | LessEqual | Greater | GreaterEqual => FieldComparison { location },
                And | Or | Xor | ShiftRight | ShiftLeft => FieldBitwiseOp { location },
                Modulo => FieldModulo { location },
                other => unreachable!("Operator {other:?} should be valid for Field"),
            });
        }

        use_impl
    }

    // Given a binary operator and another type. This method will produce the output type
    // and a boolean indicating whether to use the trait impl corresponding to the operator
    // or not. A value of false indicates the caller to use a primitive operation for this
    // operator, while a true value indicates a user-provided trait impl is required.
    pub(super) fn infix_operand_type_rules(
        &mut self,
        lhs_type: &Type,
        op: &HirBinaryOp,
        rhs_type: &Type,
        location: Location,
    ) -> Result<(Type, bool), TypeCheckError> {
        if op.kind.is_comparator() {
            return self.comparator_operand_type_rules(lhs_type, rhs_type, op, location);
        }

        use Type::*;
        match (lhs_type, rhs_type) {
            // An error type on either side will always return an error
            (Error, _) | (_, Error) => Ok((Error, false)),
            (Alias(alias, args), other) | (other, Alias(alias, args)) => {
                let alias = alias.borrow().get_type(args);
                self.infix_operand_type_rules(&alias, op, other, location)
            }

            // Matches on TypeVariable must be first so that we follow any type
            // bindings.
            (TypeVariable(int), other) | (other, TypeVariable(int)) => {
                if op.kind == BinaryOpKind::ShiftLeft || op.kind == BinaryOpKind::ShiftRight {
                    self.unify(
                        rhs_type,
                        &Type::Integer(Signedness::Unsigned, IntegerBitSize::Eight),
                        || TypeCheckError::InvalidShiftSize { location },
                    );
                    let use_impl = if lhs_type.is_numeric_value() {
                        let integer_type = self.polymorphic_integer();
                        self.bind_type_variables_for_infix(lhs_type, op, &integer_type, location)
                    } else {
                        true
                    };
                    return Ok((lhs_type.clone(), use_impl));
                }
                if let TypeBinding::Bound(binding) = &*int.borrow() {
                    return self.infix_operand_type_rules(binding, op, other, location);
                }
                let use_impl = self.bind_type_variables_for_infix(lhs_type, op, rhs_type, location);
                Ok((other.clone(), use_impl))
            }
            (Integer(sign_x, bit_width_x), Integer(sign_y, bit_width_y)) => {
                if op.kind == BinaryOpKind::ShiftLeft || op.kind == BinaryOpKind::ShiftRight {
                    if *sign_y != Signedness::Unsigned || *bit_width_y != IntegerBitSize::Eight {
                        return Err(TypeCheckError::InvalidShiftSize { location });
                    }
                    return Ok((Integer(*sign_x, *bit_width_x), false));
                }
                if sign_x != sign_y {
                    return Err(TypeCheckError::IntegerSignedness {
                        sign_x: *sign_x,
                        sign_y: *sign_y,
                        location,
                    });
                }
                if bit_width_x != bit_width_y {
                    return Err(TypeCheckError::IntegerBitWidth {
                        bit_width_x: *bit_width_x,
                        bit_width_y: *bit_width_y,
                        location,
                    });
                }
                Ok((Integer(*sign_x, *bit_width_x), false))
            }
            // The result of two Fields is always a witness
            (FieldElement, FieldElement) => {
                if !op.kind.is_valid_for_field_type() {
                    if op.kind == BinaryOpKind::Modulo {
                        return Err(TypeCheckError::FieldModulo { location });
                    } else {
                        return Err(TypeCheckError::FieldBitwiseOp { location });
                    }
                }
                Ok((FieldElement, false))
            }

            (Bool, Bool) => match op.kind {
                BinaryOpKind::Add
                | BinaryOpKind::Subtract
                | BinaryOpKind::Multiply
                | BinaryOpKind::Divide
                | BinaryOpKind::ShiftRight
                | BinaryOpKind::ShiftLeft
                | BinaryOpKind::Modulo => {
                    Err(TypeCheckError::InvalidBoolInfixOp { op: op.kind, location })
                }
                BinaryOpKind::Equal
                | BinaryOpKind::NotEqual
                | BinaryOpKind::Less
                | BinaryOpKind::LessEqual
                | BinaryOpKind::Greater
                | BinaryOpKind::GreaterEqual
                | BinaryOpKind::And
                | BinaryOpKind::Or
                | BinaryOpKind::Xor => Ok((Bool, false)),
            },

            (lhs, rhs) => {
                if op.kind == BinaryOpKind::ShiftLeft || op.kind == BinaryOpKind::ShiftRight {
                    if rhs == &Type::Integer(Signedness::Unsigned, IntegerBitSize::Eight) {
                        return Ok((lhs.clone(), true));
                    }
                    return Err(TypeCheckError::InvalidShiftSize { location });
                }
                self.unify(lhs, rhs, || TypeCheckError::TypeMismatchWithSource {
                    expected: lhs.clone(),
                    actual: rhs.clone(),
                    location: op.location,
                    source: Source::Binary,
                });
                Ok((lhs.clone(), true))
            }
        }
    }

    // Given a unary operator and a type, this method will produce the output type
    // and a boolean indicating whether to use the trait impl corresponding to the operator
    // or not. A value of false indicates the caller to use a primitive operation for this
    // operator, while a true value indicates a user-provided trait impl is required.
    pub(super) fn prefix_operand_type_rules(
        &mut self,
        op: &UnaryOp,
        rhs_type: &Type,
        location: Location,
    ) -> Result<(Type, bool), TypeCheckError> {
        use Type::*;

        match op {
            crate::ast::UnaryOp::Minus | crate::ast::UnaryOp::Not => {
                match rhs_type {
                    // An error type will always return an error
                    Error => Ok((Error, false)),
                    Alias(alias, args) => {
                        let alias = alias.borrow().get_type(args);
                        self.prefix_operand_type_rules(op, &alias, location)
                    }

                    // Matches on TypeVariable must be first so that we follow any type
                    // bindings.
                    TypeVariable(int) => {
                        if let TypeBinding::Bound(binding) = &*int.borrow() {
                            return self.prefix_operand_type_rules(op, binding, location);
                        }

                        // The `!` prefix operator is not valid for Field, so if this is a numeric
                        // type we constrain it to just (non-Field) integer types.
                        if matches!(op, crate::ast::UnaryOp::Not) && rhs_type.is_numeric_value() {
                            let integer_type = Type::polymorphic_integer(self.interner);
                            self.unify(rhs_type, &integer_type, || {
                                TypeCheckError::InvalidUnaryOp {
                                    typ: rhs_type.to_string(),
                                    operator: "!",
                                    location,
                                }
                            });
                        }

                        Ok((rhs_type.clone(), !rhs_type.is_numeric_value()))
                    }
                    Integer(sign_x, bit_width_x) => {
                        if *op == UnaryOp::Minus && *sign_x == Signedness::Unsigned {
                            return Err(TypeCheckError::InvalidUnaryOp {
                                typ: rhs_type.to_string(),
                                operator: "-",
                                location,
                            });
                        }
                        Ok((Integer(*sign_x, *bit_width_x), false))
                    }
                    // The result of a Field is always a witness
                    FieldElement => {
                        if *op == UnaryOp::Not {
                            return Err(TypeCheckError::FieldNot { location });
                        }
                        Ok((FieldElement, false))
                    }

                    Bool => Ok((Bool, false)),

                    _ => Ok((rhs_type.clone(), true)),
                }
            }
            crate::ast::UnaryOp::Reference { mutable } => {
                let typ = Type::Reference(Box::new(rhs_type.follow_bindings()), *mutable);
                Ok((typ, false))
            }
            crate::ast::UnaryOp::Dereference { implicitly_added: _ } => {
                let element_type = self.interner.next_type_variable();
                let make_expected =
                    |mutable| Type::Reference(Box::new(element_type.clone()), mutable);

                let immutable = make_expected(false);
                let mutable = make_expected(true);

                // Both `&mut T` and `&T` should coerce to an expected `&T`.
                if !rhs_type.try_reference_coercion(&immutable) {
                    self.unify(rhs_type, &mutable, || TypeCheckError::TypeMismatch {
                        expr_typ: rhs_type.to_string(),
                        expected_typ: mutable.to_string(),
                        expr_location: location,
                    });
                }
                Ok((element_type, false))
            }
        }
    }

    /// Prerequisite: verify_trait_constraint of the operator's trait constraint.
    ///
    /// Although by this point the operator is expected to already have a trait impl,
    /// we still need to match the operator's type against the method's instantiated type
    /// to ensure the instantiation bindings are correct and the monomorphizer can
    /// re-apply the needed bindings.
    pub(super) fn type_check_operator_method(
        &mut self,
        expr_id: ExprId,
        trait_method_id: TraitItemId,
        object_type: &Type,
        location: Location,
    ) {
        let method_type = self.interner.definition_type(trait_method_id.item_id);
        let (method_type, mut bindings) = method_type.instantiate(self.interner);

        match method_type {
            Type::Function(args, _, _, _) => {
                // We can cheat a bit and match against only the object type here since no operator
                // overload uses other generic parameters or return types aside from the object type.
                let expected_object_type = &args[0];
                self.unify(object_type, expected_object_type, || TypeCheckError::TypeMismatch {
                    expected_typ: expected_object_type.to_string(),
                    expr_typ: object_type.to_string(),
                    expr_location: location,
                });
            }
            other => {
                unreachable!("Expected operator method to have a function type, but found {other}")
            }
        }

        // We must also remember to apply these substitutions to the object_type
        // referenced by the selected trait impl, if one has yet to be selected.
        let impl_kind = self.interner.get_selected_impl_for_expression(expr_id);
        if let Some(TraitImplKind::Assumed { object_type, trait_generics }) = impl_kind {
            let the_trait = self.interner.get_trait(trait_method_id.trait_id);
            let object_type = object_type.substitute(&bindings);
            bindings.insert(
                the_trait.self_type_typevar.id(),
                (
                    the_trait.self_type_typevar.clone(),
                    the_trait.self_type_typevar.kind(),
                    object_type.clone(),
                ),
            );

            self.interner.select_impl_for_expression(
                expr_id,
                TraitImplKind::Assumed { object_type, trait_generics },
            );
        }

        self.interner.store_instantiation_bindings(expr_id, bindings);
    }

    pub(super) fn type_check_member_access(
        &mut self,
        mut access: HirMemberAccess,
        expr_id: ExprId,
        lhs_type: Type,
        location: Location,
    ) -> Type {
        let access_lhs = &mut access.lhs;

        let dereference_lhs = |this: &mut Self, lhs_type, element| {
            let old_lhs = *access_lhs;
            *access_lhs = this.interner.push_expr(HirExpression::Prefix(HirPrefixExpression::new(
                crate::ast::UnaryOp::Dereference { implicitly_added: true },
                old_lhs,
            )));
            this.interner.push_expr_type(old_lhs, lhs_type);
            this.interner.push_expr_type(*access_lhs, element);

            let old_location = this.interner.id_location(old_lhs);
            let location = Location::new(location.span, old_location.file);
            this.interner.push_expr_location(*access_lhs, location);
        };

        // If this access is just a field offset, we want to avoid dereferencing
        let dereference_lhs = (!access.is_offset).then_some(dereference_lhs);

        match self.check_field_access(&lhs_type, access.rhs.as_str(), location, dereference_lhs) {
            Some((element_type, index)) => {
                self.interner.set_field_index(expr_id, index);
                // We must update `access` in case we added any dereferences to it
                self.interner.replace_expr(&expr_id, HirExpression::MemberAccess(access));
                element_type
            }
            None => Type::Error,
        }
    }

    pub(crate) fn lookup_method(
        &mut self,
        object_type: &Type,
        method_name: &str,
        location: Location,
        object_location: Location,
        check_self_param: bool,
    ) -> Option<HirMethodReference> {
        match object_type.follow_bindings() {
            // TODO: We should allow method calls on `impl Trait`s eventually.
            //       For now it is fine since they are only allowed on return types.
            Type::TraitAsType(..) => {
                self.push_err(TypeCheckError::UnresolvedMethodCall {
                    method_name: method_name.to_string(),
                    object_type: object_type.clone(),
                    location,
                });
                None
            }
            Type::NamedGeneric(_) => self.lookup_method_in_trait_constraints(
                object_type,
                method_name,
                location,
                object_location,
            ),
            // References to another type should resolve to methods of their element type.
            // This may be a data type or a primitive type.
            Type::Reference(element, _mutable) => self.lookup_method(
                &element,
                method_name,
                location,
                object_location,
                check_self_param,
            ),

            // If we fail to resolve the object to a data type, we have no way of type
            // checking its arguments as we can't even resolve the name of the function
            Type::Error => None,

            // The type variable must be unbound at this point since follow_bindings was called
            Type::TypeVariable(var) if var.kind() == Kind::Normal => {
                self.push_err(TypeCheckError::TypeAnnotationsNeededForMethodCall { location });
                None
            }

            other => self.lookup_type_or_primitive_method(
                &other,
                method_name,
                location,
                object_location,
                check_self_param,
            ),
        }
    }

    fn lookup_type_or_primitive_method(
        &mut self,
        object_type: &Type,
        method_name: &str,
        location: Location,
        object_location: Location,
        check_self_param: bool,
    ) -> Option<HirMethodReference> {
        // First search in the type methods. If there is one, that's the one.
        if let Some(method_id) =
            self.interner.lookup_direct_method(object_type, method_name, check_self_param)
        {
            return Some(HirMethodReference::FuncId(method_id));
        }

        // Next lookup all matching trait methods.
        let trait_methods =
            self.interner.lookup_trait_methods(object_type, method_name, check_self_param);

        // If there's at least one matching trait method we need to see if only one is in scope.
        if !trait_methods.is_empty() {
            return self.return_trait_method_in_scope(&trait_methods, method_name, location);
        }

        // If we couldn't find any trait methods, search in
        // impls for all types `T`, e.g. `impl<T> Foo for T`
        let generic_methods =
            self.interner.lookup_generic_methods(object_type, method_name, check_self_param);
        if !generic_methods.is_empty() {
            return self.return_trait_method_in_scope(&generic_methods, method_name, location);
        }

        if let Type::DataType(datatype, _) = object_type {
            let datatype = datatype.borrow();
            let mut has_field_with_function_type = false;

            if let Some(fields) = datatype.fields_raw() {
                has_field_with_function_type = fields
                    .iter()
                    .any(|field| field.name.as_str() == method_name && field.typ.is_function());
            }

            if has_field_with_function_type {
                self.push_err(TypeCheckError::CannotInvokeStructFieldFunctionType {
                    method_name: method_name.to_string(),
                    object_type: object_type.clone(),
                    location,
                });
            } else {
                self.push_err(TypeCheckError::UnresolvedMethodCall {
                    method_name: method_name.to_string(),
                    object_type: object_type.clone(),
                    location,
                });
            }
            None
        } else {
            // It could be that this type is a composite type that is bound to a trait,
            // for example `x: (T, U) ... where (T, U): SomeTrait`
            // (so this case is a generalization of the NamedGeneric case)
            self.lookup_method_in_trait_constraints(
                object_type,
                method_name,
                location,
                object_location,
            )
        }
    }

    /// Given a list of functions and the trait they belong to, returns the one function
    /// that is in scope.
    fn return_trait_method_in_scope(
        &mut self,
        trait_methods: &[(FuncId, TraitId)],
        method_name: &str,
        location: Location,
    ) -> Option<HirMethodReference> {
        let (method, error) = self.get_trait_method_in_scope(trait_methods, method_name, location);
        if let Some(error) = error {
            self.push_err(error);
        }
        method
    }

    fn get_trait_method_in_scope(
        &mut self,
        trait_methods: &[(FuncId, TraitId)],
        method_name: &str,
        location: Location,
    ) -> (Option<HirMethodReference>, Option<PathResolutionError>) {
        let module_id = self.module_id();
        let module_data = self.get_module(module_id);

        // Only keep unique trait IDs: multiple trait methods might come from the same trait
        // but implemented with different generics (like `Convert<Field>` and `Convert<i32>`).
        let traits: HashSet<TraitId> =
            trait_methods.iter().map(|(_, trait_id)| *trait_id).collect();

        let traits_in_scope: Vec<_> = traits
            .iter()
            .filter_map(|trait_id| {
                module_data.find_trait_in_scope(*trait_id).map(|name| (*trait_id, name.clone()))
            })
            .collect();

        for (_, trait_name) in &traits_in_scope {
            self.usage_tracker.mark_as_used(module_id, trait_name);
        }

        if traits_in_scope.is_empty() {
            if traits.len() == 1 {
                // This is the backwards-compatible case where there's a single trait but it's not in scope
                let trait_id = *traits.iter().next().unwrap();
                let trait_ = self.interner.get_trait(trait_id);
                let trait_name = self.fully_qualified_trait_path(trait_);
                let method =
                    self.trait_hir_method_reference(trait_id, trait_methods, method_name, location);
                let error = PathResolutionError::TraitMethodNotInScope {
                    ident: Ident::new(method_name.into(), location),
                    trait_name,
                };
                return (Some(method), Some(error));
            } else {
                let traits = vecmap(traits, |trait_id| {
                    let trait_ = self.interner.get_trait(trait_id);
                    self.fully_qualified_trait_path(trait_)
                });
                let method = None;
                let error = PathResolutionError::UnresolvedWithPossibleTraitsToImport {
                    ident: Ident::new(method_name.into(), location),
                    traits,
                };
                return (method, Some(error));
            }
        }

        if traits_in_scope.len() > 1 {
            let traits = vecmap(traits, |trait_id| {
                let trait_ = self.interner.get_trait(trait_id);
                self.fully_qualified_trait_path(trait_)
            });
            let method = None;
            let error = PathResolutionError::MultipleTraitsInScope {
                ident: Ident::new(method_name.into(), location),
                traits,
            };
            return (method, Some(error));
        }

        let trait_id = traits_in_scope[0].0;
        let method =
            self.trait_hir_method_reference(trait_id, trait_methods, method_name, location);
        let error = None;
        (Some(method), error)
    }

    fn trait_hir_method_reference(
        &self,
        trait_id: TraitId,
        trait_methods: &[(FuncId, TraitId)],
        method_name: &str,
        location: Location,
    ) -> HirMethodReference {
        // If we find a single trait impl method, return it so we don't have to later determine the impl
        if trait_methods.len() == 1 {
            let (func_id, _) = trait_methods[0];
            return HirMethodReference::FuncId(func_id);
        }

        // Return a TraitMethodId with unbound generics. These will later be bound by the type-checker.
        let trait_ = self.interner.get_trait(trait_id);
        let generics = trait_.get_trait_generics(location);
        let trait_method_id = trait_.find_method(method_name, self.interner).unwrap();
        HirMethodReference::TraitItemId(trait_method_id, trait_id, generics, false)
    }

    fn lookup_method_in_trait_constraints(
        &mut self,
        object_type: &Type,
        method_name: &str,
        location: Location,
        object_location: Location,
    ) -> Option<HirMethodReference> {
        let func_id = match self.current_item {
            Some(DependencyId::Function(id)) => id,
            _ => panic!("unexpected method outside a function: {method_name}"),
        };
        let func_meta = self.interner.function_meta(&func_id);

        // If inside a trait method, check if it's a method on `self`
        if let Some(trait_id) = func_meta.trait_id {
            if Some(object_type) == self.self_type.as_ref() {
                let the_trait = self.interner.get_trait(trait_id);
                let constraint = the_trait.as_constraint(the_trait.name.location());
                if let Some(HirMethodReference::TraitItemId(method_id, trait_id, generics, _)) =
                    self.lookup_method_in_trait(
                        the_trait,
                        method_name,
                        &constraint.trait_bound,
                        the_trait.id,
                    )
                {
                    // If it is, it's an assumed trait
                    // Note that here we use the `trait_id` from `TraitItemId` because looking a method on a trait
                    // might return a method on a parent trait.
                    return Some(HirMethodReference::TraitItemId(
                        method_id, trait_id, generics, true,
                    ));
                }
            }
        }

        for constraint in func_meta.all_trait_constraints() {
            if *object_type == constraint.typ {
                if let Some(the_trait) =
                    self.interner.try_get_trait(constraint.trait_bound.trait_id)
                {
                    if let Some(method) = self.lookup_method_in_trait(
                        the_trait,
                        method_name,
                        &constraint.trait_bound,
                        the_trait.id,
                    ) {
                        return Some(method);
                    }
                }
            }
        }

        if object_type.is_bindable() {
            self.push_err(TypeCheckError::TypeAnnotationsNeededForMethodCall {
                location: object_location,
            });
        } else {
            self.push_err(TypeCheckError::UnresolvedMethodCall {
                method_name: method_name.to_string(),
                object_type: object_type.clone(),
                location,
            });
        }

        None
    }

    fn lookup_method_in_trait(
        &self,
        the_trait: &Trait,
        method_name: &str,
        trait_bound: &ResolvedTraitBound,
        starting_trait_id: TraitId,
    ) -> Option<HirMethodReference> {
        if let Some(trait_method) = the_trait.find_method(method_name, self.interner) {
            let trait_generics = trait_bound.trait_generics.clone();
            let trait_item_id =
                HirMethodReference::TraitItemId(trait_method, the_trait.id, trait_generics, false);
            return Some(trait_item_id);
        }

        // Search in the parent traits, if any
        for parent_trait_bound in &the_trait.trait_bounds {
            if let Some(the_trait) = self.interner.try_get_trait(parent_trait_bound.trait_id) {
                // Avoid looping forever in case there are cycles
                if the_trait.id == starting_trait_id {
                    continue;
                }

                let parent_trait_bound =
                    self.instantiate_parent_trait_bound(trait_bound, parent_trait_bound);
                if let Some(method) = self.lookup_method_in_trait(
                    the_trait,
                    method_name,
                    &parent_trait_bound,
                    starting_trait_id,
                ) {
                    return Some(method);
                }
            }
        }

        None
    }

    pub(super) fn type_check_call(
        &mut self,
        call: &HirCallExpression,
        func_type: Type,
        args: Vec<(Type, ExprId, Location)>,
        location: Location,
    ) -> Type {
        self.run_lint(|elaborator| {
            lints::deprecated_function(elaborator.interner, call.func).map(Into::into)
        });

        let is_current_func_constrained = self.in_constrained_function();

        let func_type_is_unconstrained =
            if let Type::Function(_args, _ret, _env, unconstrained) = &func_type {
                *unconstrained
            } else {
                false
            };

        let is_unconstrained_call =
            func_type_is_unconstrained || self.is_unconstrained_call(call.func);
        let crossing_runtime_boundary = is_current_func_constrained && is_unconstrained_call;
        if crossing_runtime_boundary {
            match self.unsafe_block_status {
                UnsafeBlockStatus::NotInUnsafeBlock => {
                    self.push_err(TypeCheckError::Unsafe { location });
                }
                UnsafeBlockStatus::InUnsafeBlockWithoutUnconstrainedCalls => {
                    self.unsafe_block_status = UnsafeBlockStatus::InUnsafeBlockWithConstrainedCalls;
                }
                UnsafeBlockStatus::InUnsafeBlockWithConstrainedCalls => (),
            }

            if let Some(called_func_id) = self.interner.lookup_function_from_expr(&call.func) {
                self.run_lint(|elaborator| {
                    lints::oracle_called_from_constrained_function(
                        elaborator.interner,
                        &called_func_id,
                        is_current_func_constrained,
                        location,
                    )
                    .map(Into::into)
                });
            }

            let errors = lints::unconstrained_function_args(&args);
            for error in errors {
                self.push_err(error);
            }
        }

        let return_type = self.bind_function_type(func_type, args, location);

        if crossing_runtime_boundary {
            self.run_lint(|_| {
                lints::unconstrained_function_return(&return_type, location).map(Into::into)
            });
        }

        return_type
    }

    fn is_unconstrained_call(&self, expr: ExprId) -> bool {
        if let Some(func_id) = self.interner.lookup_function_from_expr(&expr) {
            let modifiers = self.interner.function_modifiers(&func_id);
            modifiers.is_unconstrained
        } else {
            false
        }
    }

    /// Check if the given method type requires a mutable reference to the object type, and check
    /// if the given object type is already a mutable reference. If not, add one.
    /// This is used to automatically transform a method call: `foo.bar()` into a function
    /// call: `bar(&mut foo)`.
    ///
    /// A notable corner case of this function is where it interacts with auto-deref of `.`.
    /// If a field is being mutated e.g. `foo.bar.mutate_bar()` where `foo: &mut Foo`, the compiler
    /// will insert a dereference before bar `(*foo).bar.mutate_bar()` which would cause us to
    /// mutate a copy of bar rather than a reference to it. We must check for this corner case here
    /// and remove the implicitly added dereference operator if we find one.
    pub(super) fn try_add_mutable_reference_to_object(
        &mut self,
        function_type: &Type,
        object_type: &mut Type,
        object: &mut ExprId,
    ) {
        let expected_object_type = match function_type {
            Type::Function(args, _, _, _) => args.first(),
            Type::Forall(_, typ) => match typ.as_ref() {
                Type::Function(args, _, _, _) => args.first(),
                typ => unreachable!("Unexpected type for function: {typ}"),
            },
            typ => unreachable!("Unexpected type for function: {typ}"),
        };

        if let Some(expected_object_type) = expected_object_type {
            let actual_type = object_type.follow_bindings();

            if let Type::Reference(_, mutable) = expected_object_type.follow_bindings() {
                if !matches!(actual_type, Type::Reference(..)) {
                    let location = self.interner.id_location(*object);
                    self.check_can_mutate(*object, location);

                    let new_type = Type::Reference(Box::new(actual_type), mutable);
                    *object_type = new_type.clone();

                    // First try to remove a dereference operator that may have been implicitly
                    // inserted by a field access expression `foo.bar` on a mutable reference `foo`.
                    let new_object = self.try_remove_implicit_dereference(*object);

                    // If that didn't work, then wrap the whole expression in an `&mut`
                    *object = new_object.unwrap_or_else(|| {
                        let new_object = self.interner.push_expr(HirExpression::Prefix(
                            HirPrefixExpression::new(UnaryOp::Reference { mutable }, *object),
                        ));
                        self.interner.push_expr_type(new_object, new_type);
                        self.interner.push_expr_location(new_object, location);
                        new_object
                    });
                }
            // Otherwise if the object type is a mutable reference and the method is not, insert as
            // many dereferences as needed.
            } else if matches!(actual_type, Type::Reference(..)) {
                let (new_object, new_type) = self.insert_auto_dereferences(*object, actual_type);
                *object_type = new_type;
                *object = new_object;
            }
        }
    }

    pub fn type_check_function_body(&mut self, body_type: Type, meta: &FuncMeta, body_id: ExprId) {
        let (expr_location, empty_function) = self.function_info(body_id);
        let declared_return_type = meta.return_type();

        let func_location = self.interner.expr_location(&body_id); // XXX: We could be more specific and return the span of the last stmt, however stmts do not have spans yet
        if let Type::TraitAsType(trait_id, _, generics) = declared_return_type {
            if self
                .interner
                .lookup_trait_implementation(
                    &body_type,
                    *trait_id,
                    &generics.ordered,
                    &generics.named,
                )
                .is_err()
            {
                self.push_err(TypeCheckError::TypeMismatchWithSource {
                    expected: declared_return_type.clone(),
                    actual: body_type,
                    location: func_location,
                    source: Source::Return(meta.return_type.clone(), expr_location),
                });
            }
        } else {
            self.unify_with_coercions(
                &body_type,
                declared_return_type,
                body_id,
                func_location,
                || {
                    let mut error = TypeCheckError::TypeMismatchWithSource {
                        expected: declared_return_type.clone(),
                        actual: body_type.clone(),
                        location: func_location,
                        source: Source::Return(meta.return_type.clone(), expr_location),
                    };

                    if empty_function {
                        error = error.add_context(
                        "implicitly returns `()` as its body has no tail or `return` expression",
                    );
                    }
                    CompilationError::TypeError(error)
                },
            );
        }
    }

    fn function_info(&self, function_body_id: ExprId) -> (noirc_errors::Location, bool) {
        let (expr_location, empty_function) =
            if let HirExpression::Block(block) = self.interner.expression(&function_body_id) {
                let last_stmt = block.statements().last();
                let mut location = self.interner.expr_location(&function_body_id);

                if let Some(last_stmt) = last_stmt {
                    if let HirStatement::Expression(expr) = self.interner.statement(last_stmt) {
                        location = self.interner.expr_location(&expr);
                    }
                }

                (location, last_stmt.is_none())
            } else {
                (self.interner.expr_location(&function_body_id), false)
            };
        (expr_location, empty_function)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn verify_trait_constraint(
        &mut self,
        object_type: &Type,
        trait_id: TraitId,
        trait_generics: &[Type],
        associated_types: &[NamedType],
        function_ident_id: ExprId,
        select_impl: bool,
        location: Location,
    ) {
        match self.interner.lookup_trait_implementation(
            object_type,
            trait_id,
            trait_generics,
            associated_types,
        ) {
            Ok((impl_kind, instantiation_bindings)) => {
                if select_impl {
                    // Insert any additional instantiation bindings into this expression's
                    // instantiation bindings. We should avoid doing this if `select_impl` is
                    // not true since that means we're not solving for this expressions exact
                    // impl anyway. If we ignore this, we may rarely overwrite existing type
                    // bindings causing incorrect types. The `slice_regex` test is one example
                    // of that happening without this being behind `select_impl`.
                    let mut bindings =
                        self.interner.get_instantiation_bindings(function_ident_id).clone();

                    // These can clash in the `slice_regex` test which causes us to insert
                    // incorrect type bindings if they override the previous bindings.
                    for (id, binding) in instantiation_bindings {
                        let existing = bindings.insert(id, binding.clone());

                        if let Some((_, type_var, existing)) = existing {
                            let existing = existing.follow_bindings();
                            let new = binding.2.follow_bindings();

                            // Exact equality on types is intentional here, we never want to
                            // overwrite even type variables but should probably avoid a panic if
                            // the types are exactly the same.
                            if existing != new {
                                panic!(
                                    "Overwriting an existing type binding with a different type!\n  {type_var:?} <- {existing:?}\n  {type_var:?} <- {new:?}"
                                );
                            }
                        }
                    }

                    self.interner.store_instantiation_bindings(function_ident_id, bindings);
                    self.interner.select_impl_for_expression(function_ident_id, impl_kind);
                }
            }
            Err(error) => self.push_trait_constraint_error(object_type, error, location),
        }
    }

    pub(super) fn push_trait_constraint_error(
        &mut self,
        object_type: &Type,
        error: ImplSearchErrorKind,
        location: Location,
    ) {
        match error {
            ImplSearchErrorKind::TypeAnnotationsNeededOnObjectType => {
                self.push_err(TypeCheckError::TypeAnnotationsNeededForMethodCall { location });
            }
            ImplSearchErrorKind::Nested(constraints) => {
                if let Some(error) =
                    NoMatchingImplFoundError::new(self.interner, constraints, location)
                {
                    self.push_err(TypeCheckError::NoMatchingImplFound(error));
                }
            }
            ImplSearchErrorKind::MultipleMatching(candidates) => {
                let object_type = object_type.clone();
                let err =
                    TypeCheckError::MultipleMatchingImpls { object_type, location, candidates };
                self.push_err(err);
            }
        }
    }

    pub fn add_existing_generics(
        &mut self,
        unresolved_generics: &UnresolvedGenerics,
        generics: &Generics,
    ) {
        assert_eq!(unresolved_generics.len(), generics.len());

        for (unresolved_generic, generic) in unresolved_generics.iter().zip(generics) {
            self.add_existing_generic(unresolved_generic, unresolved_generic.location(), generic);
        }
    }

    pub fn add_existing_generic(
        &mut self,
        unresolved_generic: &UnresolvedGeneric,
        location: Location,
        resolved_generic: &ResolvedGeneric,
    ) {
        if let Some(name) = unresolved_generic.ident().ident() {
            let name = name.as_str();

            if let Some(generic) = self.find_generic(name) {
                self.push_err(ResolverError::DuplicateDefinition {
                    name: name.to_string(),
                    first_location: generic.location,
                    second_location: location,
                });
            } else {
                self.generics.push(resolved_generic.clone());
            }
        }
    }

    pub fn bind_generics_from_trait_constraint(
        &self,
        constraint: &TraitConstraint,
        assumed: bool,
        bindings: &mut TypeBindings,
    ) {
        self.bind_generics_from_trait_bound(&constraint.trait_bound, bindings);

        // If the trait impl is already assumed to exist we should add any type bindings for `Self`.
        // Otherwise `self` will be replaced with a fresh type variable, which will require the user
        // to specify a redundant type annotation.
        if assumed {
            let the_trait = self.interner.get_trait(constraint.trait_bound.trait_id);
            let self_type = the_trait.self_type_typevar.clone();
            let kind = the_trait.self_type_typevar.kind();
            bindings.insert(self_type.id(), (self_type, kind, constraint.typ.clone()));
        }
    }

    pub fn bind_generics_from_trait_bound(
        &self,
        trait_bound: &ResolvedTraitBound,
        bindings: &mut TypeBindings,
    ) {
        let the_trait = self.interner.get_trait(trait_bound.trait_id);

        bind_ordered_generics(&the_trait.generics, &trait_bound.trait_generics.ordered, bindings);

        let associated_types = the_trait.associated_types.clone();
        bind_named_generics(associated_types, &trait_bound.trait_generics.named, bindings);
    }

    pub fn instantiate_parent_trait_bound(
        &self,
        trait_bound: &ResolvedTraitBound,
        parent_trait_bound: &ResolvedTraitBound,
    ) -> ResolvedTraitBound {
        let mut bindings = TypeBindings::default();
        self.bind_generics_from_trait_bound(trait_bound, &mut bindings);
        ResolvedTraitBound {
            trait_generics: parent_trait_bound.trait_generics.map(|typ| typ.substitute(&bindings)),
            ..*parent_trait_bound
        }
    }

    pub(crate) fn fully_qualified_trait_path(&self, trait_: &Trait) -> String {
        let module_def_id = ModuleDefId::TraitId(trait_.id);
        let visibility = trait_.visibility;
        let defining_module = None;
        let trait_is_visible = module_def_id_is_visible(
            module_def_id,
            self.module_id(),
            visibility,
            defining_module,
            self.interner,
            self.def_maps,
            &self.crate_graph[self.crate_id].dependencies,
        );

        if !trait_is_visible {
            let dependencies = &self.crate_graph[self.crate_id].dependencies;

            for reexport in self.interner.get_trait_reexports(trait_.id) {
                let reexport_is_visible = module_def_id_is_visible(
                    module_def_id,
                    self.module_id(),
                    reexport.visibility,
                    Some(reexport.module_id),
                    self.interner,
                    self.def_maps,
                    dependencies,
                );
                if reexport_is_visible {
                    let module_path = fully_qualified_module_path(
                        self.def_maps,
                        self.crate_graph,
                        &self.crate_id,
                        reexport.module_id,
                    );
                    return format!("{module_path}::{}", reexport.name);
                }
            }

            if let Some(reexport) = get_ancestor_module_reexport(
                module_def_id,
                visibility,
                self.module_id(),
                self.interner,
                self.def_maps,
                dependencies,
            ) {
                let module_path = fully_qualified_module_path(
                    self.def_maps,
                    self.crate_graph,
                    &self.crate_id,
                    reexport.module_id,
                );
                return format!("{module_path}::{}::{}", reexport.name, trait_.name);
            }
        }

        fully_qualified_module_path(self.def_maps, self.crate_graph, &self.crate_id, trait_.id.0)
    }
}

pub(crate) fn bind_ordered_generics(
    params: &[ResolvedGeneric],
    args: &[Type],
    bindings: &mut TypeBindings,
) {
    assert_eq!(params.len(), args.len());

    for (param, arg) in params.iter().zip(args) {
        bind_generic(param, arg, bindings);
    }
}

pub(crate) fn bind_named_generics(
    mut params: Vec<ResolvedGeneric>,
    args: &[NamedType],
    bindings: &mut TypeBindings,
) {
    assert!(args.len() <= params.len());

    for arg in args {
        let i = params
            .iter()
            .position(|typ| *typ.name == arg.name.as_str())
            .unwrap_or_else(|| unreachable!("Expected to find associated type named {}", arg.name));

        let param = params.swap_remove(i);
        bind_generic(&param, &arg.typ, bindings);
    }

    for unbound_param in params {
        bind_generic(&unbound_param, &Type::Error, bindings);
    }
}

fn bind_generic(param: &ResolvedGeneric, arg: &Type, bindings: &mut TypeBindings) {
    // Avoid binding t = t
    if !arg.occurs(param.type_var.id()) {
        bindings.insert(param.type_var.id(), (param.type_var.clone(), param.kind(), arg.clone()));
    }
}
