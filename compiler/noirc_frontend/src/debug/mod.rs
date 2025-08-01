use crate::ast::PathSegment;
use crate::parse_program;
use crate::parser::{ParsedModule, ParsedSubModule};
use crate::signed_field::SignedField;
use crate::{ast, ast::Path, parser::ItemKind};
use fm::FileId;
use noirc_errors::debug_info::{DebugFnId, DebugFunction};
use noirc_errors::{Location, Span};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::mem::take;

const MAX_MEMBER_ASSIGN_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct SourceVarId(pub u32);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct SourceFieldId(pub u32);

/// This structure is used to collect information about variables to track
/// for debugging during the instrumentation injection phase.
#[derive(Debug, Clone)]
pub struct DebugInstrumenter {
    // all collected variable names while instrumenting the source for variable tracking
    pub variables: HashMap<SourceVarId, String>,

    // all field names referenced when assigning to a member of a variable
    pub field_names: HashMap<SourceFieldId, String>,

    // all collected function metadata (name + argument names)
    pub functions: HashMap<DebugFnId, DebugFunction>,

    next_var_id: u32,
    next_field_name_id: u32,
    next_fn_id: u32,

    // last seen variable names and their IDs grouped by scope
    scope: Vec<HashMap<String, SourceVarId>>,
}

impl Default for DebugInstrumenter {
    fn default() -> Self {
        Self {
            variables: HashMap::default(),
            field_names: HashMap::default(),
            functions: HashMap::default(),
            scope: vec![],
            next_var_id: 0,
            next_field_name_id: 1,
            next_fn_id: 0,
        }
    }
}

impl DebugInstrumenter {
    pub fn instrument_module(&mut self, module: &mut ParsedModule, file: FileId) {
        module.items.iter_mut().for_each(|item| {
            match &mut item.kind {
                // Instrument top-level functions of a module
                ItemKind::Function(f) => self.walk_fn(&mut f.def),
                // Instrument contract module
                ItemKind::Submodules(ParsedSubModule {
                    is_contract: true,
                    contents: contract_module,
                    ..
                }) => {
                    self.instrument_module(contract_module, file);
                }
                _ => (),
            }
        });

        // this part absolutely must happen after ast traversal above
        // so that oracle functions don't get wrapped, resulting in infinite recursion:
        self.insert_state_set_oracle(module, file);
    }

    fn insert_var(&mut self, var_name: &str) -> Option<SourceVarId> {
        if var_name == "_" {
            return None;
        }

        let var_id = SourceVarId(self.next_var_id);
        self.next_var_id += 1;
        self.variables.insert(var_id, var_name.to_string());
        self.scope.last_mut().unwrap().insert(var_name.to_string(), var_id);
        Some(var_id)
    }

    fn lookup_var(&self, var_name: &str) -> Option<SourceVarId> {
        self.scope.iter().rev().find_map(|vars| vars.get(var_name).copied())
    }

    fn insert_field_name(&mut self, field_name: &str) -> SourceFieldId {
        let field_name_id = SourceFieldId(self.next_field_name_id);
        self.next_field_name_id += 1;
        self.field_names.insert(field_name_id, field_name.to_string());
        field_name_id
    }

    fn insert_function(&mut self, fn_name: String, arguments: Vec<String>) -> DebugFnId {
        let fn_id = DebugFnId(self.next_fn_id);
        self.next_fn_id += 1;
        self.functions.insert(fn_id, DebugFunction { name: fn_name, arg_names: arguments });
        fn_id
    }

    fn walk_fn(&mut self, func: &mut ast::FunctionDefinition) {
        let func_name = func.name.to_string();
        let func_args =
            func.parameters.iter().map(|param| pattern_to_string(&param.pattern)).collect();
        let fn_id = self.insert_function(func_name, func_args);
        let enter_stmt = build_debug_call_stmt("enter", fn_id, func.location);
        self.scope.push(HashMap::default());

        let set_fn_params: Vec<_> = func
            .parameters
            .iter()
            .flat_map(|param| {
                pattern_vars(&param.pattern)
                    .iter()
                    .filter_map(|(id, _is_mut)| {
                        let var_id = self.insert_var(id.as_str())?;
                        Some(build_assign_var_stmt(var_id, id_expr(id)))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        let func_body = &mut func.body.statements;
        let mut statements = take(func_body);

        self.walk_scope(&mut statements, func.location);

        // walk_scope ensures that the last statement is the return value of the function
        let last_stmt = statements.pop().expect("at least one statement after walk_scope");
        let exit_stmt = build_debug_call_stmt("exit", fn_id, last_stmt.location);

        // rebuild function body
        func_body.push(enter_stmt);
        func_body.extend(set_fn_params);
        func_body.extend(statements);
        func_body.push(exit_stmt);
        func_body.push(last_stmt);
    }

    // Modify a vector of statements in-place, adding instrumentation for sets and drops.
    // This function will consume a scope level.
    fn walk_scope(&mut self, statements: &mut Vec<ast::Statement>, location: Location) {
        statements.iter_mut().for_each(|stmt| self.walk_statement(stmt));

        // extract and save the return value from the scope if there is one
        let ret_stmt = statements.pop();
        let has_ret_expr = match ret_stmt {
            None => false,
            Some(ast::Statement { kind: ast::StatementKind::Expression(ret_expr), .. }) => {
                let save_ret_expr = ast::Statement {
                    kind: ast::StatementKind::new_let(
                        ast::Pattern::Identifier(ident("__debug_expr", ret_expr.location)),
                        ast::UnresolvedTypeData::Unspecified.with_dummy_location(),
                        ret_expr.clone(),
                        vec![],
                    ),
                    location: ret_expr.location,
                };
                statements.push(save_ret_expr);
                true
            }
            Some(ret_stmt) => {
                // not an expression, so leave it untouched
                statements.push(ret_stmt);
                false
            }
        };

        let span = Span::empty(location.span.end());
        let location = Location::new(span, location.file);

        // drop scope variables
        let scope_vars = self.scope.pop().unwrap_or_default();
        let drop_vars_stmts =
            scope_vars.values().map(|var_id| build_drop_var_stmt(*var_id, location));
        statements.extend(drop_vars_stmts);

        // return the saved value in __debug_expr, or unit otherwise
        let last_stmt = if has_ret_expr {
            ast::Statement {
                kind: ast::StatementKind::Expression(ast::Expression {
                    kind: ast::ExpressionKind::Variable(ast::Path::plain(
                        vec![PathSegment::from(ident("__debug_expr", location))],
                        location,
                    )),
                    location,
                }),
                location,
            }
        } else {
            ast::Statement {
                kind: ast::StatementKind::Expression(ast::Expression {
                    kind: ast::ExpressionKind::Literal(ast::Literal::Unit),
                    location,
                }),
                location,
            }
        };
        statements.push(last_stmt);
    }

    fn walk_let_statement(
        &mut self,
        let_stmt: &ast::LetStatement,
        location: Location,
    ) -> ast::Statement {
        // rewrites let statements written like this:
        //   let (((a,b,c),D { d }),e,f) = x;
        //
        // into statements like this:
        //
        //   let (a,b,c,d,e,f,g) = {
        //     let (((a,b,c),D { d }),e,f) = x;
        //     wrap(1, a);
        //     wrap(2, b);
        //     ...
        //     wrap(6, f);
        //     (a,b,c,d,e,f,g)
        //   };

        // a.b.c[3].x[i*4+1].z

        let vars = pattern_vars(&let_stmt.pattern);
        let vars_pattern: Vec<ast::Pattern> = vars
            .iter()
            .map(|(id, is_mut)| {
                if *is_mut {
                    ast::Pattern::Mutable(
                        Box::new(ast::Pattern::Identifier(id.clone())),
                        id.location(),
                        true,
                    )
                } else {
                    ast::Pattern::Identifier(id.clone())
                }
            })
            .collect();
        let vars_exprs: Vec<ast::Expression> = vars
            .iter()
            .map(|(id, _)| {
                // We don't want to generate an expression to read from "_".
                // And since this expression is going to be assigned to "_" so it doesn't matter
                // what it is, we can use `()` for it.
                if id.as_str() == "_" {
                    ast::Expression {
                        kind: ast::ExpressionKind::Literal(ast::Literal::Unit),
                        location: id.location(),
                    }
                } else {
                    id_expr(id)
                }
            })
            .collect();

        let mut block_stmts =
            vec![ast::Statement { kind: ast::StatementKind::Let(let_stmt.clone()), location }];
        block_stmts.extend(vars.iter().filter_map(|(id, _)| {
            let var_id = self.insert_var(id.as_str())?;
            Some(build_assign_var_stmt(var_id, id_expr(id)))
        }));
        block_stmts.push(ast::Statement {
            kind: ast::StatementKind::Expression(ast::Expression {
                kind: ast::ExpressionKind::Tuple(vars_exprs),
                location: let_stmt.pattern.location(),
            }),
            location: let_stmt.pattern.location(),
        });

        ast::Statement {
            kind: ast::StatementKind::new_let(
                ast::Pattern::Tuple(vars_pattern, let_stmt.pattern.location()),
                ast::UnresolvedTypeData::Unspecified.with_dummy_location(),
                ast::Expression {
                    kind: ast::ExpressionKind::Block(ast::BlockExpression {
                        statements: block_stmts,
                    }),
                    location: let_stmt.expression.location,
                },
                vec![],
            ),
            location,
        }
    }

    fn walk_assign_statement(
        &mut self,
        assign_stmt: &ast::AssignStatement,
        location: Location,
    ) -> ast::Statement {
        // X = Y becomes:
        // X = {
        //   let __debug_expr = Y;
        //
        //   __debug_var_assign(17, __debug_expr);
        //   // or:
        //   __debug_member_assign_{arity}(17, __debug_expr, _v0, _v1..., _v{arity});
        //
        //   __debug_expr
        // };

        let let_kind = ast::StatementKind::new_let(
            ast::Pattern::Identifier(ident("__debug_expr", assign_stmt.expression.location)),
            ast::UnresolvedTypeData::Unspecified.with_dummy_location(),
            assign_stmt.expression.clone(),
            vec![],
        );
        let expression_location = assign_stmt.expression.location;
        let new_assign_stmt = match &assign_stmt.lvalue {
            ast::LValue::Path(id) => {
                let Some(id) = id.as_ident() else {
                    panic!("var lookup failed for var_name={id}");
                };
                let var_id = self
                    .lookup_var(id.as_str())
                    .unwrap_or_else(|| panic!("var lookup failed for var_name={id}"));
                build_assign_var_stmt(var_id, id_expr(&ident("__debug_expr", id.location())))
            }
            ast::LValue::Dereference(_lv, location) => {
                // TODO: this is a dummy statement for now, but we should
                // somehow track the dereference and update the pointed to
                // variable
                ast::Statement {
                    kind: ast::StatementKind::Expression(uint_expr(0, *location)),
                    location: *location,
                }
            }
            _ => {
                let mut indexes = vec![];
                let mut cursor = &assign_stmt.lvalue;
                let var_id;
                loop {
                    match cursor {
                        ast::LValue::Path(id) => {
                            let Some(id) = id.as_ident() else {
                                panic!("var lookup failed for var_name={id}");
                            };

                            var_id = self
                                .lookup_var(id.as_str())
                                .unwrap_or_else(|| panic!("var lookup failed for var_name={id}"));
                            break;
                        }
                        ast::LValue::MemberAccess { object, field_name, location } => {
                            cursor = object;
                            let field_name_id = self.insert_field_name(field_name.as_str());
                            indexes.push(sint_expr(-(field_name_id.0 as i128), *location));
                        }
                        ast::LValue::Index { index, array, location: _ } => {
                            cursor = array;
                            indexes.push(index.clone());
                        }
                        ast::LValue::Dereference(_ref, _span) => {
                            unimplemented![]
                        }
                        ast::LValue::Interned(..) => {
                            unimplemented![]
                        }
                    }
                }
                build_assign_member_stmt(
                    var_id,
                    &indexes,
                    &id_expr(&ident("__debug_expr", expression_location)),
                )
            }
        };

        let ret_kind =
            ast::StatementKind::Expression(id_expr(&ident("__debug_expr", expression_location)));

        ast::Statement {
            kind: ast::StatementKind::Assign(ast::AssignStatement {
                lvalue: assign_stmt.lvalue.clone(),
                expression: ast::Expression {
                    kind: ast::ExpressionKind::Block(ast::BlockExpression {
                        statements: vec![
                            ast::Statement { kind: let_kind, location: expression_location },
                            new_assign_stmt,
                            ast::Statement { kind: ret_kind, location: expression_location },
                        ],
                    }),
                    location: expression_location,
                },
            }),
            location,
        }
    }

    fn walk_expr(&mut self, expr: &mut ast::Expression) {
        match &mut expr.kind {
            ast::ExpressionKind::Block(ast::BlockExpression { statements, .. }) => {
                self.scope.push(HashMap::default());
                self.walk_scope(statements, expr.location);
            }
            ast::ExpressionKind::Prefix(prefix_expr) => {
                self.walk_expr(&mut prefix_expr.rhs);
            }
            ast::ExpressionKind::Index(index_expr) => {
                self.walk_expr(&mut index_expr.collection);
                self.walk_expr(&mut index_expr.index);
            }
            ast::ExpressionKind::Call(call_expr) => {
                // TODO: push a stack frame or something here?
                self.walk_expr(&mut call_expr.func);
                call_expr.arguments.iter_mut().for_each(|expr| {
                    self.walk_expr(expr);
                });
            }
            ast::ExpressionKind::MethodCall(mc_expr) => {
                // TODO: also push a stack frame here
                self.walk_expr(&mut mc_expr.object);
                mc_expr.arguments.iter_mut().for_each(|expr| {
                    self.walk_expr(expr);
                });
            }
            ast::ExpressionKind::Constructor(c_expr) => {
                c_expr.fields.iter_mut().for_each(|(_id, expr)| {
                    self.walk_expr(expr);
                });
            }
            ast::ExpressionKind::MemberAccess(ma_expr) => {
                self.walk_expr(&mut ma_expr.lhs);
            }
            ast::ExpressionKind::Cast(cast_expr) => {
                self.walk_expr(&mut cast_expr.lhs);
            }
            ast::ExpressionKind::Infix(infix_expr) => {
                self.walk_expr(&mut infix_expr.lhs);
                self.walk_expr(&mut infix_expr.rhs);
            }
            ast::ExpressionKind::If(if_expr) => {
                self.walk_expr(&mut if_expr.condition);
                self.walk_expr(&mut if_expr.consequence);
                if let Some(ref mut alt) = if_expr.alternative {
                    self.walk_expr(alt);
                }
            }
            ast::ExpressionKind::Tuple(exprs) => {
                exprs.iter_mut().for_each(|ref mut expr| {
                    self.walk_expr(expr);
                });
            }
            ast::ExpressionKind::Lambda(lambda) => {
                self.walk_expr(&mut lambda.body);
            }
            ast::ExpressionKind::Parenthesized(expr) => {
                self.walk_expr(expr);
            }
            _ => {}
        }
    }

    fn walk_for(&mut self, for_stmt: &mut ast::ForLoopStatement) {
        let var_name = for_stmt.identifier.as_str();
        let var_id = self.insert_var(var_name);

        let set_and_drop_stmt = var_id.map(|var_id| {
            let span = Span::empty(for_stmt.location.span.end());
            (
                build_assign_var_stmt(var_id, id_expr(&for_stmt.identifier)),
                build_drop_var_stmt(var_id, Location::new(span, for_stmt.location.file)),
            )
        });

        self.walk_expr(&mut for_stmt.block);

        let mut statements = Vec::new();
        let block_statement = ast::Statement {
            kind: ast::StatementKind::Semi(for_stmt.block.clone()),
            location: for_stmt.block.location,
        };

        if let Some((set_stmt, drop_stmt)) = set_and_drop_stmt {
            statements.push(set_stmt);
            statements.push(block_statement);
            statements.push(drop_stmt);
        } else {
            statements.push(block_statement);
        }

        for_stmt.block = ast::Expression {
            kind: ast::ExpressionKind::Block(ast::BlockExpression { statements }),
            location: for_stmt.location,
        };
    }

    fn walk_statement(&mut self, stmt: &mut ast::Statement) {
        match &mut stmt.kind {
            ast::StatementKind::Let(let_stmt) => {
                *stmt = self.walk_let_statement(let_stmt, stmt.location);
            }
            ast::StatementKind::Assign(assign_stmt) => {
                *stmt = self.walk_assign_statement(assign_stmt, stmt.location);
            }
            ast::StatementKind::Expression(expr) => {
                self.walk_expr(expr);
            }
            ast::StatementKind::Semi(expr) => {
                self.walk_expr(expr);
            }
            ast::StatementKind::For(for_stmt) => {
                self.walk_for(for_stmt);
            }
            _ => {} // Constrain, Error
        }
    }

    fn insert_state_set_oracle(&self, module: &mut ParsedModule, file: FileId) {
        let member_assigns = (1..=MAX_MEMBER_ASSIGN_DEPTH)
            .map(|i| format!["__debug_member_assign_{i}"])
            .collect::<Vec<String>>()
            .join(",\n");
        let (program, errors) = parse_program(
            &format!(
                r#"
            use __debug::{{
                __debug_var_assign,
                __debug_var_drop,
                __debug_fn_enter,
                __debug_fn_exit,
                __debug_dereference_assign,
                {member_assigns},
            }};"#
            ),
            file,
        );
        if !errors.is_empty() {
            panic!("errors parsing internal oracle definitions: {errors:?}")
        }
        module.items.extend(program.items);
    }
}

pub fn build_debug_crate_file() -> String {
    [
        r#"
            #[oracle(__debug_var_assign)]
            unconstrained fn __debug_var_assign_oracle<T>(_var_id: u32, _value: T) {}
            unconstrained fn __debug_var_assign_inner<T>(var_id: u32, value: T) {
                __debug_var_assign_oracle(var_id, value);
            }
            pub fn __debug_var_assign<T>(var_id: u32, value: T) {
                // Safety: debug context
                unsafe {
                {
                    __debug_var_assign_inner(var_id, value);
                }}
            }

            #[oracle(__debug_var_drop)]
            unconstrained fn __debug_var_drop_oracle(_var_id: u32) {}
            unconstrained fn __debug_var_drop_inner(var_id: u32) {
                __debug_var_drop_oracle(var_id);
            }
            pub fn __debug_var_drop(var_id: u32) {
                // Safety: debug context
                unsafe {
                {
                    __debug_var_drop_inner(var_id);
                }}
            }

            #[oracle(__debug_fn_enter)]
            unconstrained fn __debug_fn_enter_oracle(_fn_id: u32) {}
            unconstrained fn __debug_fn_enter_inner(fn_id: u32) {
                __debug_fn_enter_oracle(fn_id);
            }
            pub fn __debug_fn_enter(fn_id: u32) {
                // Safety: debug context
                unsafe {
                {
                    __debug_fn_enter_inner(fn_id);
                }}
            }

            #[oracle(__debug_fn_exit)]
            unconstrained fn __debug_fn_exit_oracle(_fn_id: u32) {}
            unconstrained fn __debug_fn_exit_inner(fn_id: u32) {
                __debug_fn_exit_oracle(fn_id);
            }
            pub fn __debug_fn_exit(fn_id: u32) {
                // Safety: debug context
                unsafe {
                {
                    __debug_fn_exit_inner(fn_id);
                }}
            }

            #[oracle(__debug_dereference_assign)]
            unconstrained fn __debug_dereference_assign_oracle<T>(_var_id: u32, _value: T) {}
            unconstrained fn __debug_dereference_assign_inner<T>(var_id: u32, value: T) {
                __debug_dereference_assign_oracle(var_id, value);
            }
            pub fn __debug_dereference_assign<T>(var_id: u32, value: T) {
                // Safety: debug context
                unsafe {
                {
                    __debug_dereference_assign_inner(var_id, value);
                }}
            }
        "#
        .to_string(),
        (1..=MAX_MEMBER_ASSIGN_DEPTH)
            .map(|n| {
                // The variable signature has to be generic as Noir supports using any polymorphic integer as an index.
                // If we were to set a specific type for index signatures here, such as `Field`, we will error in
                // type checking if we attempt to index with a different type such as `u8`.
                let indices =
                    (0..n).map(|i| format!["Index{i}"]).collect::<Vec<String>>().join(", ");
                let var_sig =
                    (0..n).map(|i| format!["_v{i}: Index{i}"]).collect::<Vec<String>>().join(", ");
                let vars = (0..n).map(|i| format!["_v{i}"]).collect::<Vec<String>>().join(", ");
                format!(
                    r#"
                #[oracle(__debug_member_assign_{n})]
                unconstrained fn __debug_oracle_member_assign_{n}<T, {indices}>(
                    _var_id: u32, _value: T, {var_sig}
                ) {{}}
                unconstrained fn __debug_inner_member_assign_{n}<T, {indices}>(
                    var_id: u32, value: T, {var_sig}
                ) {{
                    __debug_oracle_member_assign_{n}(var_id, value, {vars});
                }}
                pub fn __debug_member_assign_{n}<T, {indices}>(var_id: u32, value: T, {var_sig}) {{
                    /// Safety: debug context
                    unsafe {{
                        __debug_inner_member_assign_{n}(var_id, value, {vars});
                    }}
                }}

            "#
                )
            })
            .collect::<Vec<String>>()
            .join("\n"),
    ]
    .join("\n")
}

fn build_assign_var_stmt(var_id: SourceVarId, expr: ast::Expression) -> ast::Statement {
    let location = expr.location;
    let kind = ast::ExpressionKind::Call(Box::new(ast::CallExpression {
        func: Box::new(ast::Expression {
            kind: ast::ExpressionKind::Variable(ast::Path::plain(
                vec![PathSegment::from(ident("__debug_var_assign", location))],
                location,
            )),
            location,
        }),
        is_macro_call: false,
        arguments: vec![uint_expr(var_id.0 as u128, location), expr],
    }));
    ast::Statement { kind: ast::StatementKind::Semi(ast::Expression { kind, location }), location }
}

fn build_drop_var_stmt(var_id: SourceVarId, location: Location) -> ast::Statement {
    let kind = ast::ExpressionKind::Call(Box::new(ast::CallExpression {
        func: Box::new(ast::Expression {
            kind: ast::ExpressionKind::Variable(ast::Path::plain(
                vec![PathSegment::from(ident("__debug_var_drop", location))],
                location,
            )),
            location,
        }),
        is_macro_call: false,
        arguments: vec![uint_expr(var_id.0 as u128, location)],
    }));
    ast::Statement { kind: ast::StatementKind::Semi(ast::Expression { kind, location }), location }
}

fn build_assign_member_stmt(
    var_id: SourceVarId,
    indexes: &[ast::Expression],
    expr: &ast::Expression,
) -> ast::Statement {
    let arity = indexes.len();
    if arity > MAX_MEMBER_ASSIGN_DEPTH {
        unreachable!("Assignment to member exceeds maximum depth for debugging");
    }
    let location = expr.location;
    let kind = ast::ExpressionKind::Call(Box::new(ast::CallExpression {
        func: Box::new(ast::Expression {
            kind: ast::ExpressionKind::Variable(ast::Path::plain(
                vec![PathSegment::from(ident(&format!["__debug_member_assign_{arity}"], location))],
                location,
            )),
            location,
        }),
        is_macro_call: false,
        arguments: [
            vec![uint_expr(var_id.0 as u128, location)],
            vec![expr.clone()],
            indexes.iter().rev().cloned().collect(),
        ]
        .concat(),
    }));
    ast::Statement { kind: ast::StatementKind::Semi(ast::Expression { kind, location }), location }
}

fn build_debug_call_stmt(fname: &str, fn_id: DebugFnId, location: Location) -> ast::Statement {
    let kind = ast::ExpressionKind::Call(Box::new(ast::CallExpression {
        func: Box::new(ast::Expression {
            kind: ast::ExpressionKind::Variable(ast::Path::plain(
                vec![PathSegment::from(ident(&format!["__debug_fn_{fname}"], location))],
                location,
            )),
            location,
        }),
        is_macro_call: false,
        arguments: vec![uint_expr(fn_id.0 as u128, location)],
    }));
    ast::Statement { kind: ast::StatementKind::Semi(ast::Expression { kind, location }), location }
}

fn pattern_vars(pattern: &ast::Pattern) -> Vec<(ast::Ident, bool)> {
    let mut vars = vec![];
    let mut stack = VecDeque::from([(pattern, false)]);
    while stack.front().is_some() {
        let (pattern, is_mut) = stack.pop_front().unwrap();
        match pattern {
            ast::Pattern::Identifier(id) => {
                vars.push((id.clone(), is_mut));
            }
            ast::Pattern::Mutable(pattern, _, _) => {
                stack.push_back((pattern, true));
            }
            ast::Pattern::Tuple(patterns, _) => {
                stack.extend(patterns.iter().map(|pattern| (pattern, false)));
            }
            ast::Pattern::Struct(_, fields, _) => {
                stack.extend(fields.iter().map(|(_, pattern)| (pattern, is_mut)));
                vars.extend(fields.iter().map(|(id, _)| (id.clone(), false)));
            }
            ast::Pattern::Parenthesized(pattern, _) => {
                stack.push_back((pattern, false));
            }
            ast::Pattern::Interned(_, _) => (),
        }
    }
    vars
}

fn pattern_to_string(pattern: &ast::Pattern) -> String {
    match pattern {
        ast::Pattern::Identifier(id) => id.to_string(),
        ast::Pattern::Mutable(pattern, _, _) => {
            format!("mut {}", pattern_to_string(pattern.as_ref()))
        }
        ast::Pattern::Tuple(elements, _) => format!(
            "({})",
            elements.iter().map(pattern_to_string).collect::<Vec<String>>().join(", ")
        ),
        ast::Pattern::Struct(name, fields, _) => {
            format!(
                "{} {{ {} }}",
                name,
                fields
                    .iter()
                    .map(|(field_ident, field_pattern)| {
                        format!("{}: {}", field_ident, pattern_to_string(field_pattern))
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        }
        ast::Pattern::Parenthesized(pattern, _) => {
            format!("({})", pattern_to_string(pattern.as_ref()))
        }
        ast::Pattern::Interned(_, _) => "?Interned".to_string(),
    }
}

fn ident(s: &str, location: Location) -> ast::Ident {
    ast::Ident::new(s.to_string(), location)
}

fn id_expr(id: &ast::Ident) -> ast::Expression {
    ast::Expression {
        kind: ast::ExpressionKind::Variable(Path::plain(
            vec![PathSegment::from(id.clone())],
            id.location(),
        )),
        location: id.location(),
    }
}

fn uint_expr(x: u128, location: Location) -> ast::Expression {
    let value = SignedField::positive(x);
    let kind = ast::ExpressionKind::Literal(ast::Literal::Integer(value, None));
    ast::Expression { kind, location }
}

fn sint_expr(x: i128, location: Location) -> ast::Expression {
    let value = SignedField::from_signed(x);
    let kind = ast::ExpressionKind::Literal(ast::Literal::Integer(value, None));
    ast::Expression { kind, location }
}
