use noirc_arena::Index;
use noirc_errors::Location;

use crate::hir_def::expr::HirExpression;
use crate::hir_def::types::Type;

use crate::node_interner::{DefinitionId, DefinitionKind, Node, NodeInterner};

impl NodeInterner {
    /// Scans the interner for the item which is located at that [Location]
    ///
    /// The [Location] may not necessarily point to the beginning of the item
    /// so we check if the location's span is contained within the start or end
    /// of each items [Location]
    pub fn find_location_index(&self, location: Location) -> Option<impl Into<Index>> {
        let mut location_candidate: Option<(&Index, &Location)> = None;

        // Note: we can modify this in the future to not do a linear
        // scan by storing a separate map of the spans or by sorting the locations.
        for (index, interned_location) in self.id_to_location.iter() {
            if interned_location.contains(&location) {
                if let Some(current_location) = location_candidate {
                    if interned_location.span.is_smaller(&current_location.1.span) {
                        location_candidate = Some((index, interned_location));
                    }
                } else {
                    location_candidate = Some((index, interned_location));
                }
            }
        }
        location_candidate.map(|(index, _location)| *index)
    }

    /// Returns the [Type] of the expression that exists at the given location.
    pub fn type_at_location(&self, location: Location) -> Option<&Type> {
        // This is similar to `find_location_index` except that we skip indexes for which there is no type
        let mut location_candidate: Option<(&Index, &Location, &Type)> = None;

        for (index, interned_location) in self.id_to_location.iter() {
            if interned_location.contains(&location) {
                if let Some(typ) = self.try_id_type(*index) {
                    if let Some(current_location) = location_candidate {
                        if interned_location.span.is_smaller(&current_location.1.span) {
                            location_candidate = Some((index, interned_location, typ));
                        }
                    } else {
                        location_candidate = Some((index, interned_location, typ));
                    }
                }
            }
        }
        location_candidate.map(|(_index, _location, typ)| typ)
    }

    /// Returns the [Location] of the definition of the given Ident found at `location`.
    /// Returns [None] when definition is not found.
    pub fn get_definition_location_from(
        &self,
        location: Location,
        return_type_location_instead: bool,
    ) -> Option<Location> {
        // First try to find the location in the reference graph
        self.find_referenced_location(location).or_else(|| {
            // Otherwise fallback to the location indices
            self.find_location_index(location)
                .and_then(|index| self.resolve_location(index, return_type_location_instead))
                .or_else(|| self.try_resolve_trait_impl_location(location))
                .or_else(|| self.try_resolve_trait_method_declaration(location))
                .or_else(|| self.try_resolve_type_ref(location))
                .or_else(|| self.try_resolve_type_alias(location))
        })
    }

    pub fn get_declaration_location_from(&self, location: Location) -> Option<Location> {
        self.try_resolve_trait_method_declaration(location).or_else(|| {
            self.find_location_index(location)
                .and_then(|index| self.resolve_location(index, false))
                .and_then(|found_impl_location| {
                    self.try_resolve_trait_method_declaration(found_impl_location)
                })
        })
    }

    /// For a given [Index] we return [Location] to which we resolved to
    /// We currently return None for features not yet implemented
    /// TODO(#3659): LSP goto def should error when Ident at Location could not resolve
    fn resolve_location(
        &self,
        index: impl Into<Index>,
        return_type_location_instead: bool,
    ) -> Option<Location> {
        if return_type_location_instead {
            return self.get_type_location_from_index(index);
        }

        let node = self.nodes.get(index.into())?;

        match node {
            Node::Function(func) => {
                self.resolve_location(func.as_expr(), return_type_location_instead)
            }
            Node::Expression(expression) => {
                self.resolve_expression_location(expression, return_type_location_instead)
            }
            _ => None,
        }
    }

    fn get_type_location_from_index(&self, index: impl Into<Index>) -> Option<Location> {
        match self.id_type(index.into()) {
            Type::DataType(struct_type, _) => Some(struct_type.borrow().location),
            _ => None,
        }
    }

    /// Resolves the [Location] of the definition for a given [HirExpression]
    ///
    /// Note: current the code returns None because some expressions are not yet implemented.
    fn resolve_expression_location(
        &self,
        expression: &HirExpression,
        return_type_location_instead: bool,
    ) -> Option<Location> {
        match expression {
            HirExpression::Ident(ident, _) => {
                if ident.id != DefinitionId::dummy_id() {
                    let definition_info = self.definition(ident.id);
                    match definition_info.kind {
                        DefinitionKind::Function(func_id) => {
                            Some(self.function_meta(&func_id).location)
                        }
                        DefinitionKind::Local(_local_id) => Some(definition_info.location),
                        DefinitionKind::Global(_global_id) => Some(definition_info.location),
                        _ => None,
                    }
                } else {
                    None
                }
            }
            HirExpression::Constructor(expr) => {
                let struct_type = &expr.r#type.borrow();
                Some(struct_type.location)
            }
            HirExpression::MemberAccess(expr_member_access) => {
                self.resolve_struct_member_access(expr_member_access)
            }
            HirExpression::Call(expr_call) => {
                let func = expr_call.func;
                self.resolve_location(func, return_type_location_instead)
            }

            _ => None,
        }
    }

    /// Resolves the [Location] of the definition for a given [crate::hir_def::expr::HirMemberAccess]
    /// This is used to resolve the location of a struct member access.
    /// For example, in the expression `foo.bar` we want to resolve the location of `bar`
    /// to the location of the definition of `bar` in the struct `foo`.
    fn resolve_struct_member_access(
        &self,
        expr_member_access: &crate::hir_def::expr::HirMemberAccess,
    ) -> Option<Location> {
        let expr_lhs = &expr_member_access.lhs;
        let expr_rhs = &expr_member_access.rhs;

        let lhs_self_struct = match self.id_type(expr_lhs) {
            Type::DataType(struct_type, _) => struct_type,
            _ => return None,
        };

        let struct_type = lhs_self_struct.borrow();
        let field_names = struct_type.field_names()?;

        field_names.iter().find(|field_name| field_name.as_str() == expr_rhs.as_str()).map(
            |found_field_name| Location::new(found_field_name.span(), struct_type.location.file),
        )
    }

    /// Attempts to resolve [Location] of [Trait][crate::hir_def::traits::Trait] based on [Location] of [TraitImpl][crate::hir_def::traits::TraitImpl]
    /// This is used by LSP to resolve the location of a trait based on the location of a trait impl.
    ///
    /// Example:
    /// impl Foo for Bar { ... } -> trait Foo { ... }
    fn try_resolve_trait_impl_location(&self, location: Location) -> Option<Location> {
        self.trait_implementations
            .iter()
            .find(|shared_trait_impl| {
                let trait_impl = shared_trait_impl.1.borrow();
                trait_impl.file == location.file && trait_impl.ident.span().contains(&location.span)
            })
            .and_then(|shared_trait_impl| {
                let trait_impl = shared_trait_impl.1.borrow();
                self.traits.get(&trait_impl.trait_id).map(|trait_| trait_.location)
            })
    }

    /// Attempts to resolve [Location] of [Trait][crate::hir_def::traits::Trait]'s [TraitFunction][crate::hir_def::traits::TraitFunction] declaration based on the [Location] of a [TraitFunction][crate::hir_def::traits::TraitFunction] call.
    ///
    /// This is used by LSP to resolve the location.
    ///
    /// ### Example:
    /// ```nr
    /// trait HasToField {
    ///     fn to_field(self) -> Field;
    ///        ^------------------------------\
    /// }                                     |    
    ///                                       |
    /// fn main_func(x: u32) {                |
    ///     assert(x.to_field() == 15);       |
    ///               \......................./
    /// }
    /// ```
    ///
    fn try_resolve_trait_method_declaration(&self, location: Location) -> Option<Location> {
        self.func_meta
            .iter()
            .find(|(_, func_meta)| func_meta.location.contains(&location))
            .and_then(|(func_id, _func_meta)| {
                let (_, trait_id) = self.get_function_trait(func_id)?;

                let mut methods = self.traits.get(&trait_id)?.methods.iter();
                let method =
                    methods.find(|method| method.name.as_str() == self.function_name(func_id));
                method.map(|method| method.location)
            })
    }

    /// Attempts to resolve [Location] of [Type] based on [Location] of reference in code
    pub(crate) fn try_resolve_type_ref(&self, location: Location) -> Option<Location> {
        self.try_type_ref_at_location(location).and_then(|typ| match typ {
            Type::DataType(struct_typ, _) => Some(struct_typ.borrow().location),
            _ => None,
        })
    }

    pub fn try_type_ref_at_location(&self, location: Location) -> Option<Type> {
        self.type_ref_locations
            .iter()
            .find(|(_typ, type_ref_location)| type_ref_location.contains(&location))
            .map(|(typ, _type_ref_location)| typ.clone())
    }

    fn try_resolve_type_alias(&self, location: Location) -> Option<Location> {
        self.type_alias_ref
            .iter()
            .find(|(_, named_type_location)| named_type_location.span.contains(&location.span))
            .map(|(type_alias_id, _found_location)| {
                self.get_type_alias(*type_alias_id).borrow().location
            })
    }
}
