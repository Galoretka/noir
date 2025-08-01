use fm::FileId;
use noirc_errors::Location;
use rangemap::RangeMap;
use rustc_hash::FxHashMap as HashMap;

use crate::{
    ast::{FunctionDefinition, ItemVisibility},
    hir::def_map::{ModuleDefId, ModuleId},
    node_interner::{
        DefinitionId, FuncId, GlobalId, NodeInterner, ReferenceId, TraitAssociatedTypeId, TraitId,
        TypeAliasId, TypeId,
    },
};
use petgraph::prelude::NodeIndex as PetGraphIndex;

#[derive(Debug, Default)]
pub(crate) struct LocationIndices {
    map_file_to_range: HashMap<FileId, RangeMap<u32, PetGraphIndex>>,
}

impl LocationIndices {
    pub(crate) fn add_location(&mut self, location: Location, node_index: PetGraphIndex) {
        // Some location spans are empty: maybe they are from fictitious nodes?
        if location.span.start() == location.span.end() {
            return;
        }

        let range_map = self.map_file_to_range.entry(location.file).or_default();
        range_map.insert(location.span.start()..location.span.end(), node_index);
    }

    pub(crate) fn get_node_from_location(&self, location: Location) -> Option<PetGraphIndex> {
        let range_map = self.map_file_to_range.get(&location.file)?;
        Some(*range_map.get(&location.span.start())?)
    }
}

pub struct ReferencesTracker<'a> {
    interner: &'a mut NodeInterner,
}

impl<'a> ReferencesTracker<'a> {
    pub fn new(interner: &'a mut NodeInterner) -> Self {
        Self { interner }
    }

    pub(crate) fn add_reference(
        &mut self,
        module_def_id: ModuleDefId,
        location: Location,
        is_self_type: bool,
    ) {
        self.interner.add_module_def_id_reference(module_def_id, location, is_self_type);
    }
}

/// A `ModuleDefId` captured to be offered in LSP's auto-import feature.
///
/// The name of the item is stored in the key of the `auto_import_names` map in the `NodeInterner`.
#[derive(Debug, Copy, Clone)]
pub struct AutoImportEntry {
    /// The item to import.
    pub module_def_id: ModuleDefId,
    /// The item's visibility.
    pub visibility: ItemVisibility,
    /// If the item is available via a re-export, this contains the module where it's defined.
    /// For example:
    ///
    /// ```noir
    /// mod foo { // <- this is the defining module
    ///     mod bar {
    ///         pub struct Baz {} // This is the item
    ///     }
    ///     
    ///     pub use bar::Baz; // Here's the visibility
    /// }
    /// ```
    pub defining_module: Option<ModuleId>,
}

impl NodeInterner {
    pub fn reference_location(&self, reference: ReferenceId) -> Location {
        match reference {
            ReferenceId::Module(id) => self.module_attributes(id).location,
            ReferenceId::Function(id) => self.function_modifiers(&id).name_location,
            ReferenceId::Type(id) => {
                let typ = self.get_type(id);
                let typ = typ.borrow();
                Location::new(typ.name.span(), typ.location.file)
            }
            ReferenceId::StructMember(id, field_index) => {
                let struct_type = self.get_type(id);
                let struct_type = struct_type.borrow();
                let file = struct_type.location.file;
                Location::new(struct_type.field_at(field_index).name.span(), file)
            }
            ReferenceId::EnumVariant(id, variant_index) => {
                let typ = self.get_type(id);
                let typ = typ.borrow();
                let file = typ.location.file;
                Location::new(typ.variant_at(variant_index).name.span(), file)
            }
            ReferenceId::Trait(id) => {
                let trait_type = self.get_trait(id);
                Location::new(trait_type.name.span(), trait_type.location.file)
            }
            ReferenceId::TraitAssociatedType(id) => {
                let associated_type = self.get_trait_associated_type(id);
                associated_type.name.location()
            }
            ReferenceId::Global(id) => self.get_global(id).location,
            ReferenceId::Alias(id) => {
                let alias_type = self.get_type_alias(id);
                let alias_type = alias_type.borrow();
                Location::new(alias_type.name.span(), alias_type.location.file)
            }
            ReferenceId::Local(id) => self.definition(id).location,
            ReferenceId::Reference(location, _) => location,
        }
    }

    pub(crate) fn add_module_def_id_reference(
        &mut self,
        def_id: ModuleDefId,
        location: Location,
        is_self_type: bool,
    ) {
        match def_id {
            ModuleDefId::ModuleId(module_id) => {
                self.add_module_reference(module_id, location);
            }
            ModuleDefId::FunctionId(func_id) => {
                self.add_function_reference(func_id, location);
            }
            ModuleDefId::TypeId(type_id) => {
                self.add_type_reference(type_id, location, is_self_type);
            }
            ModuleDefId::TraitId(trait_id) => {
                self.add_trait_reference(trait_id, location, is_self_type);
            }
            ModuleDefId::TraitAssociatedTypeId(trait_associated_type_id) => {
                self.add_trait_associated_type_reference(trait_associated_type_id, location);
            }
            ModuleDefId::TypeAliasId(type_alias_id) => {
                self.add_alias_reference(type_alias_id, location);
            }
            ModuleDefId::GlobalId(global_id) => {
                self.add_global_reference(global_id, location);
            }
        };
    }

    pub(crate) fn add_module_reference(&mut self, id: ModuleId, location: Location) {
        self.add_reference(ReferenceId::Module(id), location, false);
    }

    pub(crate) fn add_type_reference(
        &mut self,
        id: TypeId,
        location: Location,
        is_self_type: bool,
    ) {
        self.add_reference(ReferenceId::Type(id), location, is_self_type);
    }

    pub(crate) fn add_struct_member_reference(
        &mut self,
        id: TypeId,
        member_index: usize,
        location: Location,
    ) {
        self.add_reference(ReferenceId::StructMember(id, member_index), location, false);
    }

    pub(crate) fn add_trait_reference(
        &mut self,
        id: TraitId,
        location: Location,
        is_self_type: bool,
    ) {
        self.add_reference(ReferenceId::Trait(id), location, is_self_type);
    }

    pub(crate) fn add_trait_associated_type_reference(
        &mut self,
        id: TraitAssociatedTypeId,
        location: Location,
    ) {
        self.add_reference(ReferenceId::TraitAssociatedType(id), location, false);
    }

    pub(crate) fn add_alias_reference(&mut self, id: TypeAliasId, location: Location) {
        self.add_reference(ReferenceId::Alias(id), location, false);
    }

    pub(crate) fn add_function_reference(&mut self, id: FuncId, location: Location) {
        self.add_reference(ReferenceId::Function(id), location, false);
    }

    pub(crate) fn add_global_reference(&mut self, id: GlobalId, location: Location) {
        self.add_reference(ReferenceId::Global(id), location, false);
    }

    pub(crate) fn add_local_reference(&mut self, id: DefinitionId, location: Location) {
        self.add_reference(ReferenceId::Local(id), location, false);
    }

    pub(crate) fn add_reference(
        &mut self,
        referenced: ReferenceId,
        location: Location,
        is_self_type: bool,
    ) {
        if !self.lsp_mode {
            return;
        }

        let reference = ReferenceId::Reference(location, is_self_type);

        let referenced_index = self.get_or_insert_reference(referenced);
        let reference_location = self.reference_location(reference);
        let reference_index = self.reference_graph.add_node(reference);

        self.reference_graph.add_edge(reference_index, referenced_index, ());
        self.location_indices.add_location(reference_location, reference_index);
    }

    pub(crate) fn add_definition_location(
        &mut self,
        referenced: ReferenceId,
        referenced_location: Location,
    ) {
        if !self.lsp_mode {
            return;
        }

        let referenced_index = self.get_or_insert_reference(referenced);
        self.location_indices.add_location(referenced_location, referenced_index);
    }

    #[tracing::instrument(skip(self), ret)]
    pub(crate) fn get_or_insert_reference(&mut self, id: ReferenceId) -> PetGraphIndex {
        if let Some(index) = self.reference_graph_indices.get(&id) {
            return *index;
        }

        let index = self.reference_graph.add_node(id);
        self.reference_graph_indices.insert(id, index);
        index
    }

    // Given a reference location, find the location of the referenced node.
    pub fn find_referenced_location(&self, reference_location: Location) -> Option<Location> {
        self.location_indices
            .get_node_from_location(reference_location)
            .and_then(|node_index| self.referenced_index(node_index))
            .map(|node_index| self.reference_location(self.reference_graph[node_index]))
    }

    // Returns the `ReferenceId` that exists at a given location, if any.
    pub fn reference_at_location(&self, location: Location) -> Option<ReferenceId> {
        self.location_indices.get_node_from_location(location)?;

        let node_index = self.location_indices.get_node_from_location(location)?;
        Some(self.reference_graph[node_index])
    }

    // Starting at the given location, find the node referenced by it. Then, gather
    // all locations that reference that node, and return all of them
    // (the references and optionally the referenced node if `include_referenced` is true).
    // If `include_self_type_name` is true, references where "Self" is written are returned,
    // otherwise they are not.
    // Returns `None` if the location is not known to this interner.
    pub fn find_all_references(
        &self,
        location: Location,
        include_referenced: bool,
        include_self_type_name: bool,
    ) -> Option<Vec<Location>> {
        let referenced_node = self.find_referenced(location)?;
        let referenced_node_index = self.reference_graph_indices[&referenced_node];

        let found_locations = self.find_all_references_for_index(
            referenced_node_index,
            include_referenced,
            include_self_type_name,
        );

        Some(found_locations)
    }

    // Returns the `ReferenceId` that is referenced by the given location, if any.
    pub fn find_referenced(&self, location: Location) -> Option<ReferenceId> {
        let node_index = self.location_indices.get_node_from_location(location)?;

        let reference_node = self.reference_graph[node_index];
        if let ReferenceId::Reference(_, _) = reference_node {
            let node_index = self.referenced_index(node_index)?;
            Some(self.reference_graph[node_index])
        } else {
            Some(reference_node)
        }
    }

    // Given a referenced node index, find all references to it and return their locations, optionally together
    // with the reference node's location if `include_referenced` is true.
    // If `include_self_type_name` is true, references where "Self" is written are returned,
    // otherwise they are not.
    fn find_all_references_for_index(
        &self,
        referenced_node_index: PetGraphIndex,
        include_referenced: bool,
        include_self_type_name: bool,
    ) -> Vec<Location> {
        let id = self.reference_graph[referenced_node_index];
        let mut edit_locations = Vec::new();
        if include_referenced && (include_self_type_name || !id.is_self_type_name()) {
            edit_locations.push(self.reference_location(id));
        }

        self.reference_graph
            .neighbors_directed(referenced_node_index, petgraph::Direction::Incoming)
            .for_each(|reference_node_index| {
                let id = self.reference_graph[reference_node_index];
                if include_self_type_name || !id.is_self_type_name() {
                    edit_locations.push(self.reference_location(id));
                }
            });
        edit_locations
    }

    // Given a reference index, returns the referenced index, if any.
    fn referenced_index(&self, reference_index: PetGraphIndex) -> Option<PetGraphIndex> {
        self.reference_graph
            .neighbors_directed(reference_index, petgraph::Direction::Outgoing)
            .next()
    }

    pub(crate) fn register_module(
        &mut self,
        id: ModuleId,
        location: Location,
        visibility: ItemVisibility,
        name: String,
    ) {
        self.add_definition_location(ReferenceId::Module(id), location);
        self.register_name_for_auto_import(name, ModuleDefId::ModuleId(id), visibility, None);
    }

    pub(crate) fn register_global(
        &mut self,
        id: GlobalId,
        name: String,
        location: Location,
        visibility: ItemVisibility,
    ) {
        self.add_definition_location(ReferenceId::Global(id), location);
        self.register_name_for_auto_import(name, ModuleDefId::GlobalId(id), visibility, None);
    }

    pub(crate) fn register_type(
        &mut self,
        id: TypeId,
        name: String,
        location: Location,
        visibility: ItemVisibility,
    ) {
        self.add_definition_location(ReferenceId::Type(id), location);
        self.register_name_for_auto_import(name, ModuleDefId::TypeId(id), visibility, None);
    }

    pub(crate) fn register_trait(
        &mut self,
        id: TraitId,
        name: String,
        location: Location,
        visibility: ItemVisibility,
    ) {
        self.add_definition_location(ReferenceId::Trait(id), location);
        self.register_name_for_auto_import(name, ModuleDefId::TraitId(id), visibility, None);
    }

    pub(crate) fn register_type_alias(
        &mut self,
        id: TypeAliasId,
        name: String,
        location: Location,
        visibility: ItemVisibility,
    ) {
        self.add_definition_location(ReferenceId::Alias(id), location);
        self.register_name_for_auto_import(name, ModuleDefId::TypeAliasId(id), visibility, None);
    }

    pub(crate) fn register_function(&mut self, id: FuncId, func_def: &FunctionDefinition) {
        let name = func_def.name.to_string();
        let id = ModuleDefId::FunctionId(id);
        self.register_name_for_auto_import(name, id, func_def.visibility, None);
    }

    pub fn register_name_for_auto_import(
        &mut self,
        name: String,
        module_def_id: ModuleDefId,
        visibility: ItemVisibility,
        defining_module: Option<ModuleId>,
    ) {
        if !self.lsp_mode {
            return;
        }

        let entry = self.auto_import_names.entry(name).or_default();
        entry.push(AutoImportEntry { module_def_id, visibility, defining_module });
    }

    #[allow(clippy::type_complexity)]
    pub fn get_auto_import_names(&self) -> &HashMap<String, Vec<AutoImportEntry>> {
        &self.auto_import_names
    }
}
