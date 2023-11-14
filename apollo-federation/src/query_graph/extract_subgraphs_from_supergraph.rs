use crate::error::{graphql_name, FederationError, SingleFederationError};
use crate::link::federation_spec_definition::{FederationSpecDefinition, FEDERATION_VERSIONS};
use crate::link::join_spec_definition::{
    FieldDirectiveArguments, JoinSpecDefinition, TypeDirectiveArguments, JOIN_VERSIONS,
};
use crate::link::link_spec_definition::LinkSpecDefinition;
use crate::link::spec::{Identity, Version};
use crate::link::spec_definition::{spec_definitions, SpecDefinition};
use crate::schema::position::{
    DirectiveDefinitionPosition, EnumTypeDefinitionPosition, InputObjectFieldDefinitionPosition,
    InputObjectTypeDefinitionPosition, InterfaceTypeDefinitionPosition,
    ObjectFieldDefinitionPosition, ObjectOrInterfaceFieldDefinitionPosition,
    ObjectOrInterfaceTypeDefinitionPosition, ObjectTypeDefinitionPosition,
    ScalarTypeDefinitionPosition, SchemaRootDefinitionKind, SchemaRootDefinitionPosition,
    TypeDefinitionPosition, UnionTypeDefinitionPosition,
};
use crate::schema::FederationSchema;
use apollo_compiler::ast::FieldDefinition;
use apollo_compiler::schema::{
    Component, ComponentName, ComponentOrigin, DirectiveDefinition, DirectiveList,
    DirectiveLocation, EnumType, EnumValueDefinition, ExtendedType, ExtensionId, InputObjectType,
    InputValueDefinition, InterfaceType, Name, NamedType, ObjectType, ScalarType, Type, UnionType,
};
use apollo_compiler::{name, Node, NodeStr, Schema};
use indexmap::{IndexMap, IndexSet};
use lazy_static::lazy_static;
use std::collections::BTreeMap;
use std::ops::Deref;

// Assumes the given schema has been validated.
//
// TODO: A lot of common data gets passed around in the functions called by this one, considering
// making an e.g. ExtractSubgraphs struct to contain the data.
pub(super) fn extract_subgraphs_from_supergraph(
    supergraph_schema: Schema,
    validate_extracted_subgraphs: Option<bool>,
) -> Result<FederationSubgraphs, FederationError> {
    let validate_extracted_subgraphs = validate_extracted_subgraphs.unwrap_or(true);
    let (supergraph_schema, link_spec_definition, join_spec_definition) =
        validate_supergraph(supergraph_schema)?;
    let is_fed_1 = *join_spec_definition.version() == Version { major: 0, minor: 1 };
    let (mut subgraphs, federation_spec_definitions, graph_enum_value_name_to_subgraph_name) =
        collect_empty_subgraphs(&supergraph_schema, join_spec_definition)?;

    let mut filtered_types = Vec::new();
    for type_definition_position in supergraph_schema.get_types() {
        if !join_spec_definition
            .is_spec_type_name(&supergraph_schema, type_definition_position.type_name())?
            && !link_spec_definition
                .is_spec_type_name(&supergraph_schema, type_definition_position.type_name())?
        {
            filtered_types.push(type_definition_position);
        }
    }
    if is_fed_1 {
        // Handle Fed 1 supergraphs eventually, the extraction logic is gnarly
        todo!()
    } else {
        extract_subgraphs_from_fed_2_supergraph(
            &supergraph_schema,
            &mut subgraphs,
            &graph_enum_value_name_to_subgraph_name,
            &federation_spec_definitions,
            join_spec_definition,
            &filtered_types,
        )?;
    }

    for graph_enum_value in graph_enum_value_name_to_subgraph_name.keys() {
        let subgraph = get_subgraph(
            &mut subgraphs,
            &graph_enum_value_name_to_subgraph_name,
            graph_enum_value,
        )?;
        let federation_spec_definition = federation_spec_definitions
            .get(graph_enum_value)
            .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
                message: "Subgraph unexpectedly does not use federation spec".to_owned(),
            })?;
        add_federation_operations(subgraph, federation_spec_definition)?;
        if validate_extracted_subgraphs {
            let Some(diagnostics) = subgraph.schema.schema().validate().err() else {
                continue;
            };
            // TODO: Implement maybeDumpSubgraphSchema() for better error messaging
            if is_fed_1 {
                // See message above about Fed 1 supergraphs
                todo!()
            } else {
                return Err(
                    SingleFederationError::InvalidFederationSupergraph {
                        message: format!(
                            "Unexpected error extracting {} from the supergraph: this is either a bug, or the supergraph has been corrupted.\n\nDetails:\n{}",
                            subgraph.name,
                            diagnostics.to_string_no_color()
                        ),
                    }.into()
                );
            }
        }
    }

    Ok(subgraphs)
}

type ValidateSupergraphOk = (
    FederationSchema,
    &'static LinkSpecDefinition,
    &'static JoinSpecDefinition,
);

fn validate_supergraph(supergraph_schema: Schema) -> Result<ValidateSupergraphOk, FederationError> {
    let supergraph_schema = FederationSchema::new(supergraph_schema)?;
    let Some(metadata) = supergraph_schema.metadata() else {
        return Err(SingleFederationError::InvalidFederationSupergraph {
            message: "Invalid supergraph: must be a core schema".to_owned(),
        }
        .into());
    };
    let link_spec_definition = metadata.link_spec_definition()?;
    let Some(join_link) = metadata.for_identity(&Identity::join_identity()) else {
        return Err(SingleFederationError::InvalidFederationSupergraph {
            message: "Invalid supergraph: must use the join spec".to_owned(),
        }
        .into());
    };
    let Some(join_spec_definition) =
        spec_definitions(JOIN_VERSIONS.deref())?.find(&join_link.url.version)
    else {
        return Err(SingleFederationError::InvalidFederationSupergraph {
            message: format!(
                "Invalid supergraph: uses unsupported join spec version {} (supported versions: {})",
                spec_definitions(JOIN_VERSIONS.deref())?.versions().map( | v| v.to_string()).collect:: < Vec<String> > ().join(", "),
                join_link.url.version,
            ),
        }.into());
    };
    Ok((
        supergraph_schema,
        link_spec_definition,
        join_spec_definition,
    ))
}

type CollectEmptySubgraphsOk = (
    FederationSubgraphs,
    IndexMap<Name, &'static FederationSpecDefinition>,
    IndexMap<Name, NodeStr>,
);
fn collect_empty_subgraphs(
    supergraph_schema: &FederationSchema,
    join_spec_definition: &JoinSpecDefinition,
) -> Result<CollectEmptySubgraphsOk, FederationError> {
    let mut subgraphs = FederationSubgraphs::new();
    let graph_directive_definition =
        join_spec_definition.graph_directive_definition(supergraph_schema)?;
    let graph_enum = join_spec_definition.graph_enum_definition(supergraph_schema)?;
    let mut federation_spec_definitions = IndexMap::new();
    let mut graph_enum_value_name_to_subgraph_name = IndexMap::new();
    for (enum_value_name, enum_value_definition) in graph_enum.values.iter() {
        let graph_application = enum_value_definition
            .directives
            .iter()
            .find(|d| d.name == graph_directive_definition.name)
            .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
                message: format!(
                    "Value \"{}\" of join__Graph enum has no @join__graph directive",
                    enum_value_name
                ),
            })?;
        let graph_arguments = join_spec_definition.graph_directive_arguments(graph_application)?;
        let subgraph = FederationSubgraph {
            name: graph_arguments.name.as_str().to_owned(),
            url: graph_arguments.url.as_str().to_owned(),
            schema: new_empty_fed_2_subgraph_schema()?,
        };
        let federation_link = &subgraph
            .schema
            .metadata()
            .as_ref()
            .and_then(|metadata| metadata.for_identity(&Identity::federation_identity()))
            .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
                message: "Subgraph unexpectedly does not use federation spec".to_owned(),
            })?;
        let federation_spec_definition = spec_definitions(FEDERATION_VERSIONS.deref())?
            .find(&federation_link.url.version)
            .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
                message: "Subgraph unexpectedly does not use a supported federation spec version"
                    .to_owned(),
            })?;
        subgraphs.add(subgraph)?;
        graph_enum_value_name_to_subgraph_name
            .insert(enum_value_name.clone(), graph_arguments.name);
        federation_spec_definitions.insert(enum_value_name.clone(), federation_spec_definition);
    }
    Ok((
        subgraphs,
        federation_spec_definitions,
        graph_enum_value_name_to_subgraph_name,
    ))
}

// TODO: Use the JS/programmatic approach instead of hard-coding definitions.
pub(crate) fn new_empty_fed_2_subgraph_schema() -> Result<FederationSchema, FederationError> {
    FederationSchema::new(Schema::parse(
        r#"
    extend schema
        @link(url: "https://specs.apollo.dev/link/v1.0")
        @link(url: "https://specs.apollo.dev/federation/v2.5")

    directive @link(url: String, as: String, for: link__Purpose, import: [link__Import]) repeatable on SCHEMA

    scalar link__Import

    enum link__Purpose {
        """
        \`SECURITY\` features provide metadata necessary to securely resolve fields.
        """
        SECURITY

        """
        \`EXECUTION\` features provide metadata necessary for operation execution.
        """
        EXECUTION
    }

    directive @federation__key(fields: federation__FieldSet!, resolvable: Boolean = true) repeatable on OBJECT | INTERFACE

    directive @federation__requires(fields: federation__FieldSet!) on FIELD_DEFINITION

    directive @federation__provides(fields: federation__FieldSet!) on FIELD_DEFINITION

    directive @federation__external(reason: String) on OBJECT | FIELD_DEFINITION

    directive @federation__tag(name: String!) repeatable on FIELD_DEFINITION | OBJECT | INTERFACE | UNION | ARGUMENT_DEFINITION | SCALAR | ENUM | ENUM_VALUE | INPUT_OBJECT | INPUT_FIELD_DEFINITION | SCHEMA

    directive @federation__extends on OBJECT | INTERFACE

    directive @federation__shareable on OBJECT | FIELD_DEFINITION

    directive @federation__inaccessible on FIELD_DEFINITION | OBJECT | INTERFACE | UNION | ARGUMENT_DEFINITION | SCALAR | ENUM | ENUM_VALUE | INPUT_OBJECT | INPUT_FIELD_DEFINITION

    directive @federation__override(from: String!) on FIELD_DEFINITION

    directive @federation__composeDirective(name: String) repeatable on SCHEMA

    directive @federation__interfaceObject on OBJECT

    directive @federation__authenticated on FIELD_DEFINITION | OBJECT | INTERFACE | SCALAR | ENUM

    directive @federation__requiresScopes(scopes: [[federation__Scope!]!]!) on FIELD_DEFINITION | OBJECT | INTERFACE | SCALAR | ENUM

    scalar federation__FieldSet

    scalar federation__Scope
    "#,
        "subgraph.graphql",
    ))
}

struct TypeInfo {
    name: NamedType,
    // HashMap<subgraph_enum_value: String, is_interface_object: bool>
    subgraph_info: IndexMap<Name, bool>,
}

struct TypeInfos {
    object_types: Vec<TypeInfo>,
    interface_types: Vec<TypeInfo>,
    union_types: Vec<TypeInfo>,
    enum_types: Vec<TypeInfo>,
    input_object_types: Vec<TypeInfo>,
}

fn extract_subgraphs_from_fed_2_supergraph(
    supergraph_schema: &FederationSchema,
    subgraphs: &mut FederationSubgraphs,
    graph_enum_value_name_to_subgraph_name: &IndexMap<Name, NodeStr>,
    federation_spec_definitions: &IndexMap<Name, &'static FederationSpecDefinition>,
    join_spec_definition: &'static JoinSpecDefinition,
    filtered_types: &Vec<TypeDefinitionPosition>,
) -> Result<(), FederationError> {
    let TypeInfos {
        object_types,
        interface_types,
        union_types,
        enum_types,
        input_object_types,
    } = add_all_empty_subgraph_types(
        supergraph_schema,
        subgraphs,
        graph_enum_value_name_to_subgraph_name,
        federation_spec_definitions,
        join_spec_definition,
        filtered_types,
    )?;

    extract_object_type_content(
        supergraph_schema,
        subgraphs,
        graph_enum_value_name_to_subgraph_name,
        federation_spec_definitions,
        join_spec_definition,
        &object_types,
    )?;
    extract_interface_type_content(
        supergraph_schema,
        subgraphs,
        graph_enum_value_name_to_subgraph_name,
        federation_spec_definitions,
        join_spec_definition,
        &interface_types,
    )?;
    extract_union_type_content(
        supergraph_schema,
        subgraphs,
        graph_enum_value_name_to_subgraph_name,
        join_spec_definition,
        &union_types,
    )?;
    extract_enum_type_content(
        supergraph_schema,
        subgraphs,
        graph_enum_value_name_to_subgraph_name,
        join_spec_definition,
        &enum_types,
    )?;
    extract_input_object_type_content(
        supergraph_schema,
        subgraphs,
        graph_enum_value_name_to_subgraph_name,
        join_spec_definition,
        &input_object_types,
    )?;

    // We add all the "executable" directive definitions from the supergraph to each subgraphs, as
    // those may be part of a query and end up in any subgraph fetches. We do this "last" to make
    // sure that if one of the directives uses a type for an argument, that argument exists. Note
    // that we don't bother with non-executable directive definitions at the moment since we
    // don't extract their applications. It might become something we need later, but we don't so
    // far. Accordingly, we skip any potentially applied directives in the argument of the copied
    // definition, because we haven't copied type-system directives.
    let all_executable_directive_definitions = supergraph_schema
        .schema()
        .directive_definitions
        .values()
        .filter_map(|directive_definition| {
            let executable_locations = directive_definition
                .locations
                .iter()
                .filter(|location| EXECUTABLE_DIRECTIVE_LOCATIONS.contains(*location))
                .copied()
                .collect::<Vec<_>>();
            if executable_locations.is_empty() {
                return None;
            }
            Some(Node::new(DirectiveDefinition {
                description: None,
                name: directive_definition.name.clone(),
                arguments: directive_definition
                    .arguments
                    .iter()
                    .map(|argument| {
                        Node::new(InputValueDefinition {
                            description: None,
                            name: argument.name.clone(),
                            ty: argument.ty.clone(),
                            default_value: argument.default_value.clone(),
                            directives: Default::default(),
                        })
                    })
                    .collect::<Vec<_>>(),
                repeatable: directive_definition.repeatable,
                locations: executable_locations,
            }))
        })
        .collect::<Vec<_>>();
    for subgraph in subgraphs.subgraphs.values_mut() {
        // TODO: removeInactiveProvidesAndRequires(subgraph.schema)
        remove_unused_types_from_subgraph(subgraph)?;
        for definition in all_executable_directive_definitions.iter() {
            DirectiveDefinitionPosition {
                directive_name: definition.name.clone(),
            }
            .insert(&mut subgraph.schema, definition.clone())?;
        }
    }

    Ok(())
}

fn add_all_empty_subgraph_types(
    supergraph_schema: &FederationSchema,
    subgraphs: &mut FederationSubgraphs,
    graph_enum_value_name_to_subgraph_name: &IndexMap<Name, NodeStr>,
    federation_spec_definitions: &IndexMap<Name, &'static FederationSpecDefinition>,
    join_spec_definition: &'static JoinSpecDefinition,
    filtered_types: &Vec<TypeDefinitionPosition>,
) -> Result<TypeInfos, FederationError> {
    let type_directive_definition =
        join_spec_definition.type_directive_definition(supergraph_schema)?;

    let mut object_types: Vec<TypeInfo> = Vec::new();
    let mut interface_types: Vec<TypeInfo> = Vec::new();
    let mut union_types: Vec<TypeInfo> = Vec::new();
    let mut enum_types: Vec<TypeInfo> = Vec::new();
    let mut input_object_types: Vec<TypeInfo> = Vec::new();

    for type_definition_position in filtered_types {
        let type_ = type_definition_position.get(supergraph_schema.schema())?;
        let mut type_directive_applications = Vec::new();
        for directive in type_.directives().iter() {
            if directive.name != type_directive_definition.name {
                continue;
            }
            type_directive_applications
                .push(join_spec_definition.type_directive_arguments(directive)?);
        }
        let types_mut = match &type_definition_position {
            TypeDefinitionPosition::Scalar(pos) => {
                // Scalar are a bit special in that they don't have any sub-component, so we don't
                // track them beyond adding them to the proper subgraphs. It's also simple because
                // there is no possible key so there is exactly one @join__type application for each
                // subgraph having the scalar (and most arguments cannot be present).
                for type_directive_application in &type_directive_applications {
                    let subgraph = get_subgraph(
                        subgraphs,
                        graph_enum_value_name_to_subgraph_name,
                        &type_directive_application.graph,
                    )?;
                    pos.pre_insert(&mut subgraph.schema)?;
                    pos.insert(
                        &mut subgraph.schema,
                        Node::new(ScalarType {
                            description: None,
                            name: pos.type_name.clone(),
                            directives: Default::default(),
                        }),
                    )?;
                }
                None
            }
            TypeDefinitionPosition::Object(_) => Some(&mut object_types),
            TypeDefinitionPosition::Interface(_) => Some(&mut interface_types),
            TypeDefinitionPosition::Union(_) => Some(&mut union_types),
            TypeDefinitionPosition::Enum(_) => Some(&mut enum_types),
            TypeDefinitionPosition::InputObject(_) => Some(&mut input_object_types),
        };
        if let Some(types_mut) = types_mut {
            types_mut.push(add_empty_type(
                type_definition_position.clone(),
                &type_directive_applications,
                subgraphs,
                graph_enum_value_name_to_subgraph_name,
                federation_spec_definitions,
            )?);
        }
    }

    Ok(TypeInfos {
        object_types,
        interface_types,
        union_types,
        enum_types,
        input_object_types,
    })
}

fn add_empty_type(
    type_definition_position: TypeDefinitionPosition,
    type_directive_applications: &Vec<TypeDirectiveArguments>,
    subgraphs: &mut FederationSubgraphs,
    graph_enum_value_name_to_subgraph_name: &IndexMap<Name, NodeStr>,
    federation_spec_definitions: &IndexMap<Name, &'static FederationSpecDefinition>,
) -> Result<TypeInfo, FederationError> {
    // In fed2, we always mark all types with `@join__type` but making sure.
    if type_directive_applications.is_empty() {
        return Err(SingleFederationError::InvalidFederationSupergraph {
            message: format!("Missing @join__type on \"{}\"", type_definition_position),
        }
        .into());
    }
    let mut type_info = TypeInfo {
        name: type_definition_position.type_name().clone(),
        subgraph_info: IndexMap::new(),
    };
    for type_directive_application in type_directive_applications {
        let subgraph = get_subgraph(
            subgraphs,
            graph_enum_value_name_to_subgraph_name,
            &type_directive_application.graph,
        )?;
        let federation_spec_definition = federation_spec_definitions
            .get(&type_directive_application.graph)
            .ok_or_else(|| SingleFederationError::Internal {
                message: format!(
                    "Missing federation spec info for subgraph enum value \"{}\"",
                    type_directive_application.graph
                ),
            })?;

        if !type_info
            .subgraph_info
            .contains_key(&type_directive_application.graph)
        {
            let mut is_interface_object = false;
            match &type_definition_position {
                TypeDefinitionPosition::Scalar(_) => {
                    return Err(SingleFederationError::Internal {
                        message: "\"add_empty_type()\" shouldn't be called for scalars".to_owned(),
                    }
                    .into());
                }
                TypeDefinitionPosition::Object(pos) => {
                    pos.pre_insert(&mut subgraph.schema)?;
                    pos.insert(
                        &mut subgraph.schema,
                        Node::new(ObjectType {
                            description: None,
                            name: pos.type_name.clone(),
                            implements_interfaces: Default::default(),
                            directives: Default::default(),
                            fields: Default::default(),
                        }),
                    )?;
                    if pos.type_name == "Query" {
                        let root_pos = SchemaRootDefinitionPosition {
                            root_kind: SchemaRootDefinitionKind::Query,
                        };
                        if root_pos.try_get(subgraph.schema.schema()).is_none() {
                            root_pos.insert(
                                &mut subgraph.schema,
                                ComponentName::from(&pos.type_name),
                            )?;
                        }
                    } else if pos.type_name == "Mutation" {
                        let root_pos = SchemaRootDefinitionPosition {
                            root_kind: SchemaRootDefinitionKind::Mutation,
                        };
                        if root_pos.try_get(subgraph.schema.schema()).is_none() {
                            root_pos.insert(
                                &mut subgraph.schema,
                                ComponentName::from(&pos.type_name),
                            )?;
                        }
                    } else if pos.type_name == "Subscription" {
                        let root_pos = SchemaRootDefinitionPosition {
                            root_kind: SchemaRootDefinitionKind::Subscription,
                        };
                        if root_pos.try_get(subgraph.schema.schema()).is_none() {
                            root_pos.insert(
                                &mut subgraph.schema,
                                ComponentName::from(&pos.type_name),
                            )?;
                        }
                    }
                }
                TypeDefinitionPosition::Interface(pos) => {
                    if type_directive_application.is_interface_object {
                        is_interface_object = true;
                        let interface_object_directive = federation_spec_definition
                            .interface_object_directive(&subgraph.schema)?;
                        let pos = ObjectTypeDefinitionPosition {
                            type_name: pos.type_name.clone(),
                        };
                        pos.pre_insert(&mut subgraph.schema)?;
                        pos.insert(
                            &mut subgraph.schema,
                            Node::new(ObjectType {
                                description: None,
                                name: pos.type_name.clone(),
                                implements_interfaces: Default::default(),
                                directives: DirectiveList(vec![Component::new(
                                    interface_object_directive,
                                )]),
                                fields: Default::default(),
                            }),
                        )?;
                    } else {
                        pos.pre_insert(&mut subgraph.schema)?;
                        pos.insert(
                            &mut subgraph.schema,
                            Node::new(InterfaceType {
                                description: None,
                                name: pos.type_name.clone(),
                                implements_interfaces: Default::default(),
                                directives: Default::default(),
                                fields: Default::default(),
                            }),
                        )?;
                    }
                }
                TypeDefinitionPosition::Union(pos) => {
                    pos.pre_insert(&mut subgraph.schema)?;
                    pos.insert(
                        &mut subgraph.schema,
                        Node::new(UnionType {
                            description: None,
                            name: pos.type_name.clone(),
                            directives: Default::default(),
                            members: Default::default(),
                        }),
                    )?;
                }
                TypeDefinitionPosition::Enum(pos) => {
                    pos.pre_insert(&mut subgraph.schema)?;
                    pos.insert(
                        &mut subgraph.schema,
                        Node::new(EnumType {
                            description: None,
                            name: pos.type_name.clone(),
                            directives: Default::default(),
                            values: Default::default(),
                        }),
                    )?;
                }
                TypeDefinitionPosition::InputObject(pos) => {
                    pos.pre_insert(&mut subgraph.schema)?;
                    pos.insert(
                        &mut subgraph.schema,
                        Node::new(InputObjectType {
                            description: None,
                            name: pos.type_name.clone(),
                            directives: Default::default(),
                            fields: Default::default(),
                        }),
                    )?;
                }
            };
            type_info.subgraph_info.insert(
                type_directive_application.graph.clone(),
                is_interface_object,
            );
        }

        if let Some(key) = &type_directive_application.key {
            let mut key_directive = Component::new(federation_spec_definition.key_directive(
                &subgraph.schema,
                key.clone(),
                type_directive_application.resolvable,
            )?);
            if type_directive_application.extension {
                key_directive.origin =
                    ComponentOrigin::Extension(ExtensionId::new(&key_directive.node))
            }
            let subgraph_type_definition_position = subgraph
                .schema
                .get_type(type_definition_position.type_name().clone())?;
            match &subgraph_type_definition_position {
                TypeDefinitionPosition::Scalar(_) => {
                    return Err(SingleFederationError::Internal {
                        message: "\"add_empty_type()\" shouldn't be called for scalars".to_owned(),
                    }
                    .into());
                }
                TypeDefinitionPosition::Object(pos) => {
                    pos.insert_directive(&mut subgraph.schema, key_directive)?;
                }
                TypeDefinitionPosition::Interface(pos) => {
                    pos.insert_directive(&mut subgraph.schema, key_directive)?;
                }
                TypeDefinitionPosition::Union(pos) => {
                    pos.insert_directive(&mut subgraph.schema, key_directive)?;
                }
                TypeDefinitionPosition::Enum(pos) => {
                    pos.insert_directive(&mut subgraph.schema, key_directive)?;
                }
                TypeDefinitionPosition::InputObject(pos) => {
                    pos.insert_directive(&mut subgraph.schema, key_directive)?;
                }
            };
        }
    }

    Ok(type_info)
}

fn extract_object_type_content(
    supergraph_schema: &FederationSchema,
    subgraphs: &mut FederationSubgraphs,
    graph_enum_value_name_to_subgraph_name: &IndexMap<Name, NodeStr>,
    federation_spec_definitions: &IndexMap<Name, &'static FederationSpecDefinition>,
    join_spec_definition: &JoinSpecDefinition,
    info: &[TypeInfo],
) -> Result<(), FederationError> {
    let field_directive_definition =
        join_spec_definition.field_directive_definition(supergraph_schema)?;
    // join__implements was added in join 0.2, and this method does not run for join 0.1, so it
    // should be defined.
    let implements_directive_definition = join_spec_definition
        .implements_directive_definition(supergraph_schema)?
        .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
            message: "@join__implements should exist for a fed2 supergraph".to_owned(),
        })?;

    for TypeInfo {
        name: type_name,
        subgraph_info,
    } in info.iter()
    {
        let pos = ObjectTypeDefinitionPosition {
            type_name: (*type_name).clone(),
        };
        let type_ = pos.get(supergraph_schema.schema())?;

        for directive in type_.directives.iter() {
            if directive.name != implements_directive_definition.name {
                continue;
            }
            let implements_directive_application =
                join_spec_definition.implements_directive_arguments(directive)?;
            if !subgraph_info.contains_key(&implements_directive_application.graph) {
                return Err(
                    SingleFederationError::InvalidFederationSupergraph {
                        message: format!(
                            "@join__implements cannot exist on \"{}\" for subgraph \"{}\" without type-level @join__type",
                            type_name,
                            implements_directive_application.graph,
                        ),
                    }.into()
                );
            }
            let subgraph = get_subgraph(
                subgraphs,
                graph_enum_value_name_to_subgraph_name,
                &implements_directive_application.graph,
            )?;
            pos.insert_implements_interface(
                &mut subgraph.schema,
                ComponentName::from(graphql_name(&implements_directive_application.interface)?),
            )?;
        }

        for (field_name, field) in type_.fields.iter() {
            let field_pos = pos.field(field_name.clone());
            let mut field_directive_applications = Vec::new();
            for directive in field.directives.iter() {
                if directive.name != field_directive_definition.name {
                    continue;
                }
                field_directive_applications
                    .push(join_spec_definition.field_directive_arguments(directive)?);
            }
            if field_directive_applications.is_empty() {
                // In a fed2 subgraph, no @join__field means that the field is in all the subgraphs
                // in which the type is.
                let is_shareable = subgraph_info.len() > 1;
                for graph_enum_value in subgraph_info.keys() {
                    let subgraph = get_subgraph(
                        subgraphs,
                        graph_enum_value_name_to_subgraph_name,
                        graph_enum_value,
                    )?;
                    let federation_spec_definition = federation_spec_definitions
                        .get(graph_enum_value)
                        .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
                            message: "Subgraph unexpectedly does not use federation spec"
                                .to_owned(),
                        })?;
                    add_subgraph_field(
                        field_pos.clone().into(),
                        field,
                        subgraph,
                        federation_spec_definition,
                        is_shareable,
                        None,
                    )?;
                }
            } else {
                let is_shareable = field_directive_applications
                    .iter()
                    .filter(|field_directive_application| {
                        !field_directive_application.external.unwrap_or(false)
                            && !field_directive_application.user_overridden.unwrap_or(false)
                    })
                    .count()
                    > 1;

                for field_directive_application in &field_directive_applications {
                    let Some(graph_enum_value) = &field_directive_application.graph else {
                        // We use a @join__field with no graph to indicates when a field in the
                        // supergraph does not come directly from any subgraph and there is thus
                        // nothing to do to "extract" it.
                        continue;
                    };
                    let subgraph = get_subgraph(
                        subgraphs,
                        graph_enum_value_name_to_subgraph_name,
                        graph_enum_value,
                    )?;
                    let federation_spec_definition = federation_spec_definitions
                        .get(graph_enum_value)
                        .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
                            message: "Subgraph unexpectedly does not use federation spec"
                                .to_owned(),
                        })?;
                    if !subgraph_info.contains_key(graph_enum_value) {
                        return Err(
                            SingleFederationError::InvalidFederationSupergraph {
                                message: format!(
                                    "@join__field cannot exist on {}.{} for subgraph {} without type-level @join__type",
                                    type_name,
                                    field_name,
                                    graph_enum_value,
                                ),
                            }.into()
                        );
                    }
                    add_subgraph_field(
                        field_pos.clone().into(),
                        field,
                        subgraph,
                        federation_spec_definition,
                        is_shareable,
                        Some(field_directive_application),
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn extract_interface_type_content(
    supergraph_schema: &FederationSchema,
    subgraphs: &mut FederationSubgraphs,
    graph_enum_value_name_to_subgraph_name: &IndexMap<Name, NodeStr>,
    federation_spec_definitions: &IndexMap<Name, &'static FederationSpecDefinition>,
    join_spec_definition: &JoinSpecDefinition,
    info: &[TypeInfo],
) -> Result<(), FederationError> {
    let field_directive_definition =
        join_spec_definition.field_directive_definition(supergraph_schema)?;
    // join_implements was added in join 0.2, and this method does not run for join 0.1, so it
    // should be defined.
    let implements_directive_definition = join_spec_definition
        .implements_directive_definition(supergraph_schema)?
        .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
            message: "@join__implements should exist for a fed2 supergraph".to_owned(),
        })?;

    for TypeInfo {
        name: type_name,
        subgraph_info,
    } in info.iter()
    {
        let type_ = InterfaceTypeDefinitionPosition {
            type_name: (*type_name).clone(),
        }
        .get(supergraph_schema.schema())?;
        fn get_pos(
            subgraph: &FederationSubgraph,
            subgraph_info: &IndexMap<Name, bool>,
            graph_enum_value: &Name,
            type_name: NamedType,
        ) -> Result<ObjectOrInterfaceTypeDefinitionPosition, FederationError> {
            let is_interface_object = *subgraph_info.get(graph_enum_value).ok_or_else(|| {
                SingleFederationError::InvalidFederationSupergraph {
                    message: format!(
                        "@join__implements cannot exist on {} for subgraph {} without type-level @join__type",
                        type_name,
                        graph_enum_value,
                    ),
                }
            })?;
            Ok(match subgraph.schema.get_type(type_name.clone())? {
                TypeDefinitionPosition::Object(pos) => {
                    if !is_interface_object {
                        return Err(
                            SingleFederationError::Internal {
                                message: "\"extract_interface_type_content()\" encountered an unexpected interface object type in subgraph".to_owned(),
                            }.into()
                        );
                    }
                    pos.into()
                }
                TypeDefinitionPosition::Interface(pos) => {
                    if is_interface_object {
                        return Err(
                            SingleFederationError::Internal {
                                message: "\"extract_interface_type_content()\" encountered an interface type in subgraph that should have been an interface object".to_owned(),
                            }.into()
                        );
                    }
                    pos.into()
                }
                _ => {
                    return Err(
                        SingleFederationError::Internal {
                            message: "\"extract_interface_type_content()\" encountered non-object/interface type in subgraph".to_owned(),
                        }.into()
                    );
                }
            })
        }

        for directive in type_.directives.iter() {
            if directive.name != implements_directive_definition.name {
                continue;
            }
            let implements_directive_application =
                join_spec_definition.implements_directive_arguments(directive)?;
            let subgraph = get_subgraph(
                subgraphs,
                graph_enum_value_name_to_subgraph_name,
                &implements_directive_application.graph,
            )?;
            let pos = get_pos(
                subgraph,
                subgraph_info,
                &implements_directive_application.graph,
                type_name.clone(),
            )?;
            match pos {
                ObjectOrInterfaceTypeDefinitionPosition::Object(pos) => {
                    pos.insert_implements_interface(
                        &mut subgraph.schema,
                        ComponentName::from(graphql_name(
                            &implements_directive_application.interface,
                        )?),
                    )?;
                }
                ObjectOrInterfaceTypeDefinitionPosition::Interface(pos) => {
                    pos.insert_implements_interface(
                        &mut subgraph.schema,
                        ComponentName::from(graphql_name(
                            &implements_directive_application.interface,
                        )?),
                    )?;
                }
            }
        }

        for (field_name, field) in type_.fields.iter() {
            let mut field_directive_applications = Vec::new();
            for directive in field.directives.iter() {
                if directive.name != field_directive_definition.name {
                    continue;
                }
                field_directive_applications
                    .push(join_spec_definition.field_directive_arguments(directive)?);
            }
            if field_directive_applications.is_empty() {
                // In a fed2 subgraph, no @join__field means that the field is in all the subgraphs
                // in which the type is.
                for graph_enum_value in subgraph_info.keys() {
                    let subgraph = get_subgraph(
                        subgraphs,
                        graph_enum_value_name_to_subgraph_name,
                        graph_enum_value,
                    )?;
                    let pos =
                        get_pos(subgraph, subgraph_info, graph_enum_value, type_name.clone())?;
                    let federation_spec_definition = federation_spec_definitions
                        .get(graph_enum_value)
                        .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
                            message: "Subgraph unexpectedly does not use federation spec"
                                .to_owned(),
                        })?;
                    add_subgraph_field(
                        pos.field(field_name.clone()),
                        field,
                        subgraph,
                        federation_spec_definition,
                        false,
                        None,
                    )?;
                }
            } else {
                for field_directive_application in &field_directive_applications {
                    let Some(graph_enum_value) = &field_directive_application.graph else {
                        // We use a @join__field with no graph to indicates when a field in the
                        // supergraph does not come directly from any subgraph and there is thus
                        // nothing to do to "extract" it.
                        continue;
                    };
                    let subgraph = get_subgraph(
                        subgraphs,
                        graph_enum_value_name_to_subgraph_name,
                        graph_enum_value,
                    )?;
                    let pos =
                        get_pos(subgraph, subgraph_info, graph_enum_value, type_name.clone())?;
                    let federation_spec_definition = federation_spec_definitions
                        .get(graph_enum_value)
                        .ok_or_else(|| SingleFederationError::InvalidFederationSupergraph {
                            message: "Subgraph unexpectedly does not use federation spec"
                                .to_owned(),
                        })?;
                    if !subgraph_info.contains_key(graph_enum_value) {
                        return Err(
                            SingleFederationError::InvalidFederationSupergraph {
                                message: format!(
                                    "@join__field cannot exist on {}.{} for subgraph {} without type-level @join__type",
                                    type_name,
                                    field_name,
                                    graph_enum_value,
                                ),
                            }.into()
                        );
                    }
                    add_subgraph_field(
                        pos.field(field_name.clone()),
                        field,
                        subgraph,
                        federation_spec_definition,
                        false,
                        Some(field_directive_application),
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn extract_union_type_content(
    supergraph_schema: &FederationSchema,
    subgraphs: &mut FederationSubgraphs,
    graph_enum_value_name_to_subgraph_name: &IndexMap<Name, NodeStr>,
    join_spec_definition: &JoinSpecDefinition,
    info: &[TypeInfo],
) -> Result<(), FederationError> {
    // This was added in join 0.3, so it can genuinely be None.
    let union_member_directive_definition =
        join_spec_definition.union_member_directive_definition(supergraph_schema)?;

    // Note that union members works a bit differently from fields or enum values, and this because
    // we cannot have directive applications on type members. So the `join_unionMember` directive
    // applications are on the type itself, and they mention the member that they target.
    for TypeInfo {
        name: type_name,
        subgraph_info,
    } in info.iter()
    {
        let pos = UnionTypeDefinitionPosition {
            type_name: (*type_name).clone(),
        };
        let type_ = pos.get(supergraph_schema.schema())?;

        let mut union_member_directive_applications = Vec::new();
        if let Some(union_member_directive_definition) = union_member_directive_definition {
            for directive in type_.directives.iter() {
                if directive.name != union_member_directive_definition.name {
                    continue;
                }
                union_member_directive_applications
                    .push(join_spec_definition.union_member_directive_arguments(directive)?);
            }
        }
        if union_member_directive_applications.is_empty() {
            // No @join__unionMember; every member should be added to every subgraph having the
            // union (at least as long as the subgraph has the member itself).
            for graph_enum_value in subgraph_info.keys() {
                let subgraph = get_subgraph(
                    subgraphs,
                    graph_enum_value_name_to_subgraph_name,
                    graph_enum_value,
                )?;
                // Note that object types in the supergraph are guaranteed to be object types in
                // subgraphs.
                let subgraph_members = type_
                    .members
                    .iter()
                    .filter(|member| {
                        subgraph
                            .schema
                            .schema()
                            .types
                            .contains_key((*member).deref())
                    })
                    .collect::<Vec<_>>();
                for member in subgraph_members {
                    pos.insert_member(&mut subgraph.schema, ComponentName::from(&member.name))?;
                }
            }
        } else {
            for union_member_directive_application in &union_member_directive_applications {
                let subgraph = get_subgraph(
                    subgraphs,
                    graph_enum_value_name_to_subgraph_name,
                    &union_member_directive_application.graph,
                )?;
                if !subgraph_info.contains_key(&union_member_directive_application.graph) {
                    return Err(
                        SingleFederationError::InvalidFederationSupergraph {
                            message: format!(
                                "@join__unionMember cannot exist on {} for subgraph {} without type-level @join__type",
                                type_name,
                                union_member_directive_application.graph,
                            ),
                        }.into()
                    );
                }
                // Note that object types in the supergraph are guaranteed to be object types in
                // subgraphs. We also know that the type must exist in this case (we don't generate
                // broken @join__unionMember).
                pos.insert_member(
                    &mut subgraph.schema,
                    ComponentName::from(graphql_name(&union_member_directive_application.member)?),
                )?;
            }
        }
    }

    Ok(())
}

fn extract_enum_type_content(
    supergraph_schema: &FederationSchema,
    subgraphs: &mut FederationSubgraphs,
    graph_enum_value_name_to_subgraph_name: &IndexMap<Name, NodeStr>,
    join_spec_definition: &JoinSpecDefinition,
    info: &[TypeInfo],
) -> Result<(), FederationError> {
    // This was added in join 0.3, so it can genuinely be None.
    let enum_value_directive_definition =
        join_spec_definition.enum_value_directive_definition(supergraph_schema)?;

    for TypeInfo {
        name: type_name,
        subgraph_info,
    } in info.iter()
    {
        let pos = EnumTypeDefinitionPosition {
            type_name: (*type_name).clone(),
        };
        let type_ = pos.get(supergraph_schema.schema())?;

        for (value_name, value) in type_.values.iter() {
            let value_pos = pos.value(value_name.clone());
            let mut enum_value_directive_applications = Vec::new();
            if let Some(enum_value_directive_definition) = enum_value_directive_definition {
                for directive in value.directives.iter() {
                    if directive.name != enum_value_directive_definition.name {
                        continue;
                    }
                    enum_value_directive_applications
                        .push(join_spec_definition.enum_value_directive_arguments(directive)?);
                }
            }
            if enum_value_directive_applications.is_empty() {
                for graph_enum_value in subgraph_info.keys() {
                    let subgraph = get_subgraph(
                        subgraphs,
                        graph_enum_value_name_to_subgraph_name,
                        graph_enum_value,
                    )?;
                    value_pos.insert(
                        &mut subgraph.schema,
                        Component::new(EnumValueDefinition {
                            description: None,
                            value: value_name.clone(),
                            directives: Default::default(),
                        }),
                    )?;
                }
            } else {
                for enum_value_directive_application in &enum_value_directive_applications {
                    let subgraph = get_subgraph(
                        subgraphs,
                        graph_enum_value_name_to_subgraph_name,
                        &enum_value_directive_application.graph,
                    )?;
                    if !subgraph_info.contains_key(&enum_value_directive_application.graph) {
                        return Err(
                            SingleFederationError::InvalidFederationSupergraph {
                                message: format!(
                                    "@join__enumValue cannot exist on {}.{} for subgraph {} without type-level @join__type",
                                    type_name,
                                    value_name,
                                    enum_value_directive_application.graph,
                                ),
                            }.into()
                        );
                    }
                    value_pos.insert(
                        &mut subgraph.schema,
                        Component::new(EnumValueDefinition {
                            description: None,
                            value: value_name.clone(),
                            directives: Default::default(),
                        }),
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn extract_input_object_type_content(
    supergraph_schema: &FederationSchema,
    subgraphs: &mut FederationSubgraphs,
    graph_enum_value_name_to_subgraph_name: &IndexMap<Name, NodeStr>,
    join_spec_definition: &JoinSpecDefinition,
    info: &[TypeInfo],
) -> Result<(), FederationError> {
    let field_directive_definition =
        join_spec_definition.field_directive_definition(supergraph_schema)?;

    for TypeInfo {
        name: type_name,
        subgraph_info,
    } in info.iter()
    {
        let pos = InputObjectTypeDefinitionPosition {
            type_name: (*type_name).clone(),
        };
        let type_ = pos.get(supergraph_schema.schema())?;

        for (input_field_name, input_field) in type_.fields.iter() {
            let input_field_pos = pos.field(input_field_name.clone());
            let mut field_directive_applications = Vec::new();
            for directive in input_field.directives.iter() {
                if directive.name != field_directive_definition.name {
                    continue;
                }
                field_directive_applications
                    .push(join_spec_definition.field_directive_arguments(directive)?);
            }
            if field_directive_applications.is_empty() {
                for graph_enum_value in subgraph_info.keys() {
                    let subgraph = get_subgraph(
                        subgraphs,
                        graph_enum_value_name_to_subgraph_name,
                        graph_enum_value,
                    )?;
                    add_subgraph_input_field(input_field_pos.clone(), input_field, subgraph, None)?;
                }
            } else {
                for field_directive_application in &field_directive_applications {
                    let Some(graph_enum_value) = &field_directive_application.graph else {
                        // We use a @join__field with no graph to indicates when a field in the
                        // supergraph does not come directly from any subgraph and there is thus
                        // nothing to do to "extract" it.
                        continue;
                    };
                    let subgraph = get_subgraph(
                        subgraphs,
                        graph_enum_value_name_to_subgraph_name,
                        graph_enum_value,
                    )?;
                    if !subgraph_info.contains_key(graph_enum_value) {
                        return Err(
                            SingleFederationError::InvalidFederationSupergraph {
                                message: format!(
                                    "@join__field cannot exist on {}.{} for subgraph {} without type-level @join__type",
                                    type_name,
                                    input_field_name,
                                    graph_enum_value,
                                ),
                            }.into()
                        );
                    }
                    add_subgraph_input_field(
                        input_field_pos.clone(),
                        input_field,
                        subgraph,
                        Some(field_directive_application),
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn add_subgraph_field(
    object_or_interface_field_definition_position: ObjectOrInterfaceFieldDefinitionPosition,
    field: &FieldDefinition,
    subgraph: &mut FederationSubgraph,
    federation_spec_definition: &'static FederationSpecDefinition,
    is_shareable: bool,
    field_directive_application: Option<&FieldDirectiveArguments>,
) -> Result<(), FederationError> {
    let field_directive_application =
        field_directive_application.unwrap_or_else(|| &FieldDirectiveArguments {
            graph: None,
            requires: None,
            provides: None,
            type_: None,
            external: None,
            override_: None,
            user_overridden: None,
        });
    let subgraph_field_type = match &field_directive_application.type_ {
        Some(t) => decode_type(t)?,
        None => field.ty.clone(),
    };
    let mut subgraph_field = FieldDefinition {
        description: None,
        name: object_or_interface_field_definition_position
            .field_name()
            .clone(),
        arguments: vec![],
        ty: subgraph_field_type,
        directives: Default::default(),
    };

    for argument in &field.arguments {
        subgraph_field
            .arguments
            .push(Node::new(InputValueDefinition {
                description: None,
                name: argument.name.clone(),
                ty: argument.ty.clone(),
                default_value: argument.default_value.clone(),
                directives: Default::default(),
            }))
    }
    if let Some(requires) = &field_directive_application.requires {
        subgraph_field.directives.push(Node::new(
            federation_spec_definition.requires_directive(&subgraph.schema, requires.clone())?,
        ));
    }
    if let Some(provides) = &field_directive_application.provides {
        subgraph_field.directives.push(Node::new(
            federation_spec_definition.provides_directive(&subgraph.schema, provides.clone())?,
        ));
    }
    let external = field_directive_application.external.unwrap_or(false);
    if external {
        subgraph_field.directives.push(Node::new(
            federation_spec_definition.external_directive(&subgraph.schema, None)?,
        ));
    }
    let user_overridden = field_directive_application.user_overridden.unwrap_or(false);
    if user_overridden {
        subgraph_field.directives.push(Node::new(
            federation_spec_definition
                .external_directive(&subgraph.schema, Some(NodeStr::new("[overridden]")))?,
        ));
    }
    if let Some(override_) = &field_directive_application.override_ {
        subgraph_field.directives.push(Node::new(
            federation_spec_definition.override_directive(&subgraph.schema, override_.clone())?,
        ));
    }
    if is_shareable && !external && !user_overridden {
        subgraph_field.directives.push(Node::new(
            federation_spec_definition.shareable_directive(&subgraph.schema)?,
        ));
    }

    match object_or_interface_field_definition_position {
        ObjectOrInterfaceFieldDefinitionPosition::Object(pos) => {
            pos.insert(&mut subgraph.schema, Component::from(subgraph_field))?;
        }
        ObjectOrInterfaceFieldDefinitionPosition::Interface(pos) => {
            pos.insert(&mut subgraph.schema, Component::from(subgraph_field))?;
        }
    };

    Ok(())
}

fn add_subgraph_input_field(
    input_object_field_definition_position: InputObjectFieldDefinitionPosition,
    input_field: &InputValueDefinition,
    subgraph: &mut FederationSubgraph,
    field_directive_application: Option<&FieldDirectiveArguments>,
) -> Result<(), FederationError> {
    let field_directive_application =
        field_directive_application.unwrap_or_else(|| &FieldDirectiveArguments {
            graph: None,
            requires: None,
            provides: None,
            type_: None,
            external: None,
            override_: None,
            user_overridden: None,
        });
    let subgraph_input_field_type = match &field_directive_application.type_ {
        Some(t) => Node::new(decode_type(t)?),
        None => input_field.ty.clone(),
    };
    let subgraph_input_field = InputValueDefinition {
        description: None,
        name: input_object_field_definition_position.field_name.clone(),
        ty: subgraph_input_field_type,
        default_value: input_field.default_value.clone(),
        directives: Default::default(),
    };

    input_object_field_definition_position
        .insert(&mut subgraph.schema, Component::from(subgraph_input_field))?;

    Ok(())
}

// TODO: Ask apollo-rs for type-reference parsing function, similar to graphql-js
fn decode_type(type_: &str) -> Result<Type, FederationError> {
    // Detect if type string is trying to end the field/type in the hack below.
    if type_.chars().any(|c| c == '}' || c == ':') {
        return Err(SingleFederationError::InvalidGraphQL {
            message: format!("Cannot parse type \"{}\"", type_),
        }
        .into());
    }
    let schema = Schema::parse(format!("type Query {{ field: {} }}", type_), "temp.graphql");
    let Some(ExtendedType::Object(dummy_type)) = schema.types.get("Query") else {
        return Err(SingleFederationError::InvalidGraphQL {
            message: format!("Cannot parse type \"{}\"", type_),
        }
        .into());
    };
    let Some(dummy_field) = dummy_type.fields.get("field") else {
        return Err(SingleFederationError::InvalidGraphQL {
            message: format!("Cannot parse type \"{}\"", type_),
        }
        .into());
    };
    Ok(dummy_field.ty.clone())
}

fn get_subgraph<'subgraph>(
    subgraphs: &'subgraph mut FederationSubgraphs,
    graph_enum_value_name_to_subgraph_name: &IndexMap<Name, NodeStr>,
    graph_enum_value: &Name,
) -> Result<&'subgraph mut FederationSubgraph, FederationError> {
    let subgraph_name = graph_enum_value_name_to_subgraph_name
        .get(graph_enum_value)
        .ok_or_else(|| {
            SingleFederationError::Internal {
                message: format!(
                    "Invalid graph enum_value \"{}\": does not match an enum value defined in the @join__Graph enum",
                    graph_enum_value,
                ),
            }
        })?;
    subgraphs.get_mut(subgraph_name).ok_or_else(|| {
        SingleFederationError::Internal {
            message: "All subgraphs should have been created by \"collect_empty_subgraphs()\""
                .to_owned(),
        }
        .into()
    })
}

pub(crate) struct FederationSubgraph {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) schema: FederationSchema,
}

pub(crate) struct FederationSubgraphs {
    subgraphs: BTreeMap<String, FederationSubgraph>,
}

impl FederationSubgraphs {
    pub(crate) fn new() -> Self {
        FederationSubgraphs {
            subgraphs: BTreeMap::new(),
        }
    }

    pub(crate) fn add(&mut self, subgraph: FederationSubgraph) -> Result<(), FederationError> {
        if self.subgraphs.contains_key(&subgraph.name) {
            return Err(SingleFederationError::InvalidFederationSupergraph {
                message: format!("A subgraph named \"{}\" already exists", subgraph.name),
            }
            .into());
        }
        self.subgraphs.insert(subgraph.name.clone(), subgraph);
        Ok(())
    }

    pub(crate) fn get(&self, name: &str) -> Option<&FederationSubgraph> {
        self.subgraphs.get(name)
    }

    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut FederationSubgraph> {
        self.subgraphs.get_mut(name)
    }
}

impl IntoIterator for FederationSubgraphs {
    type Item = <BTreeMap<String, FederationSubgraph> as IntoIterator>::Item;
    type IntoIter = <BTreeMap<String, FederationSubgraph> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.subgraphs.into_iter()
    }
}

lazy_static! {
    static ref EXECUTABLE_DIRECTIVE_LOCATIONS: IndexSet<DirectiveLocation> = {
        IndexSet::from([
            DirectiveLocation::Query,
            DirectiveLocation::Mutation,
            DirectiveLocation::Subscription,
            DirectiveLocation::Field,
            DirectiveLocation::FragmentDefinition,
            DirectiveLocation::FragmentSpread,
            DirectiveLocation::InlineFragment,
            DirectiveLocation::VariableDefinition,
        ])
    };
}

fn remove_unused_types_from_subgraph(
    subgraph: &mut FederationSubgraph,
) -> Result<(), FederationError> {
    // We now do an additional path on all types because we sometimes added types to subgraphs
    // without being sure that the subgraph had the type in the first place (especially with the
    // join 0.1 spec), and because we later might not have added any fields/members to said type,
    // they may be empty (indicating they clearly didn't belong to the subgraph in the first) and we
    // need to remove them. Note that need to do this _after_ the `add_external_fields()` call above
    // since it may have added (external) fields to some of the types.
    let mut type_definition_positions: Vec<TypeDefinitionPosition> = Vec::new();
    for (type_name, type_) in subgraph.schema.schema().types.iter() {
        match type_ {
            ExtendedType::Object(type_) => {
                if type_.fields.is_empty() {
                    type_definition_positions.push(
                        ObjectTypeDefinitionPosition {
                            type_name: type_name.clone(),
                        }
                        .into(),
                    );
                }
            }
            ExtendedType::Interface(type_) => {
                if type_.fields.is_empty() {
                    type_definition_positions.push(
                        InterfaceTypeDefinitionPosition {
                            type_name: type_name.clone(),
                        }
                        .into(),
                    );
                }
            }
            ExtendedType::Union(type_) => {
                if type_.members.is_empty() {
                    type_definition_positions.push(
                        UnionTypeDefinitionPosition {
                            type_name: type_name.clone(),
                        }
                        .into(),
                    );
                }
            }
            ExtendedType::InputObject(type_) => {
                if type_.fields.is_empty() {
                    type_definition_positions.push(
                        InputObjectTypeDefinitionPosition {
                            type_name: type_name.clone(),
                        }
                        .into(),
                    );
                }
            }
            _ => {}
        }
    }

    // Note that we have to use remove_recursive() or this could leave the subgraph invalid. But if
    // the type was not in this subgraph, nothing that depends on it should be either.
    for position in type_definition_positions {
        match position {
            TypeDefinitionPosition::Object(position) => {
                position.remove_recursive(&mut subgraph.schema)?;
            }
            TypeDefinitionPosition::Interface(position) => {
                position.remove_recursive(&mut subgraph.schema)?;
            }
            TypeDefinitionPosition::Union(position) => {
                position.remove_recursive(&mut subgraph.schema)?;
            }
            TypeDefinitionPosition::InputObject(position) => {
                position.remove_recursive(&mut subgraph.schema)?;
            }
            _ => {
                return Err(SingleFederationError::Internal {
                    message: "Encountered type kind that shouldn't have been removed".to_owned(),
                }
                .into());
            }
        }
    }

    Ok(())
}

const FEDERATION_ANY_TYPE_NAME: Name = name!("_Any");
const FEDERATION_SERVICE_TYPE_NAME: Name = name!("_Service");
const FEDERATION_SDL_FIELD_NAME: Name = name!("sdl");
const FEDERATION_ENTITY_TYPE_NAME: Name = name!("_Entity");
const FEDERATION_SERVICE_FIELD_NAME: Name = name!("_service");
const FEDERATION_ENTITIES_FIELD_NAME: Name = name!("_entities");
const FEDERATION_REPRESENTATIONS_ARGUMENTS_NAME: Name = name!("representations");

fn add_federation_operations(
    subgraph: &mut FederationSubgraph,
    federation_spec_definition: &'static FederationSpecDefinition,
) -> Result<(), FederationError> {
    // TODO: Use the JS/programmatic approach of checkOrAdd() instead of hard-coding the adds.
    let any_type_pos = ScalarTypeDefinitionPosition {
        type_name: FEDERATION_ANY_TYPE_NAME,
    };
    any_type_pos.pre_insert(&mut subgraph.schema)?;
    any_type_pos.insert(
        &mut subgraph.schema,
        Node::new(ScalarType {
            description: None,
            name: any_type_pos.type_name.clone(),
            directives: Default::default(),
        }),
    )?;
    let mut service_fields = IndexMap::new();
    service_fields.insert(
        FEDERATION_SDL_FIELD_NAME,
        Component::new(FieldDefinition {
            description: None,
            name: FEDERATION_SDL_FIELD_NAME,
            arguments: Vec::new(),
            ty: Type::Named(name!("String")),
            directives: Default::default(),
        }),
    );
    let service_type_pos = ObjectTypeDefinitionPosition {
        type_name: FEDERATION_SERVICE_TYPE_NAME,
    };
    service_type_pos.pre_insert(&mut subgraph.schema)?;
    service_type_pos.insert(
        &mut subgraph.schema,
        Node::new(ObjectType {
            description: None,
            name: service_type_pos.type_name.clone(),
            implements_interfaces: Default::default(),
            directives: Default::default(),
            fields: service_fields,
        }),
    )?;
    let key_directive_definition =
        federation_spec_definition.key_directive_definition(&subgraph.schema)?;
    let entity_members = subgraph
        .schema
        .schema()
        .types
        .iter()
        .filter_map(|(type_name, type_)| {
            let ExtendedType::Object(type_) = type_ else {
                return None;
            };
            if !type_
                .directives
                .iter()
                .any(|d| d.name == key_directive_definition.name)
            {
                return None;
            }
            Some(ComponentName::from(type_name))
        })
        .collect::<IndexSet<_>>();
    let is_entity_type = !entity_members.is_empty();
    if is_entity_type {
        let entity_type_pos = UnionTypeDefinitionPosition {
            type_name: FEDERATION_ENTITY_TYPE_NAME,
        };
        entity_type_pos.pre_insert(&mut subgraph.schema)?;
        entity_type_pos.insert(
            &mut subgraph.schema,
            Node::new(UnionType {
                description: None,
                name: entity_type_pos.type_name.clone(),
                directives: Default::default(),
                members: entity_members,
            }),
        )?;
    }

    let query_root_pos = SchemaRootDefinitionPosition {
        root_kind: SchemaRootDefinitionKind::Query,
    };
    if query_root_pos.try_get(subgraph.schema.schema()).is_none() {
        let default_query_type_pos = ObjectTypeDefinitionPosition {
            type_name: name!("Query"),
        };
        default_query_type_pos.pre_insert(&mut subgraph.schema)?;
        default_query_type_pos.insert(
            &mut subgraph.schema,
            Node::new(ObjectType {
                description: None,
                name: default_query_type_pos.type_name.clone(),
                implements_interfaces: Default::default(),
                directives: Default::default(),
                fields: Default::default(),
            }),
        )?;
        query_root_pos.insert(
            &mut subgraph.schema,
            ComponentName::from(default_query_type_pos.type_name),
        )?;
    }

    let query_root_type_name = query_root_pos.get(subgraph.schema.schema())?.name.clone();
    let entity_field_pos = ObjectFieldDefinitionPosition {
        type_name: query_root_type_name.clone(),
        field_name: FEDERATION_ENTITIES_FIELD_NAME,
    };
    if is_entity_type {
        entity_field_pos.insert(
            &mut subgraph.schema,
            Component::new(FieldDefinition {
                description: None,
                name: FEDERATION_ENTITIES_FIELD_NAME,
                arguments: vec![Node::new(InputValueDefinition {
                    description: None,
                    name: FEDERATION_REPRESENTATIONS_ARGUMENTS_NAME,
                    ty: Node::new(Type::NonNullList(Box::new(Type::NonNullNamed(
                        FEDERATION_ANY_TYPE_NAME,
                    )))),
                    default_value: None,
                    directives: Default::default(),
                })],
                ty: Type::NonNullList(Box::new(Type::Named(FEDERATION_ENTITY_TYPE_NAME))),
                directives: Default::default(),
            }),
        )?;
    } else {
        entity_field_pos.remove(&mut subgraph.schema)?;
    }

    ObjectFieldDefinitionPosition {
        type_name: query_root_type_name.clone(),
        field_name: FEDERATION_SERVICE_FIELD_NAME,
    }
    .insert(
        &mut subgraph.schema,
        Component::new(FieldDefinition {
            description: None,
            name: FEDERATION_SERVICE_FIELD_NAME,
            arguments: Vec::new(),
            ty: Type::NonNullNamed(FEDERATION_SERVICE_TYPE_NAME),
            directives: Default::default(),
        }),
    )?;

    Ok(())
}