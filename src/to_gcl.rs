//! Convert P4 to GCL

use crate::gcl::{GclAssignment, GclCommand, GclGraph, GclNode, GclNodeRange, GclPredicate};
use crate::ir::{
    IrActionDecl, IrAssignment, IrBlockStatement, IrControlDecl, IrControlLocalDecl, IrDeclaration,
    IrExpr, IrExprData, IrFunctionCall, IrIfStatement, IrInstantiation, IrProgram, IrStatement,
    IrStatementOrDecl, IrStructType, IrType, IrVariableDecl,
};
use crate::type_checker::ProgramMetadata;
use either::Either;
use petgraph::graph::NodeIndex;

/// Trait for converting a P4 AST node into GCL
pub trait ToGcl {
    type Output;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output;
}

impl ToGcl for IrProgram {
    /// The starting node
    type Output = NodeIndex;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        let start_idx = graph.add_node(GclNode {
            name: "start".to_string(),
            commands: Vec::new(),
        });

        let mut commands = Vec::new();
        let mut node_range = GclNodeRange {
            start: start_idx,
            end: start_idx,
        };
        let mut control_idx = None;

        for decl in &self.declarations {
            match decl {
                IrDeclaration::Constant(const_decl) => {
                    // Add onto the node chain
                    let range = const_decl.to_gcl(graph, metadata);
                    graph.add_edge(node_range.end, range.start, GclPredicate::default());
                    node_range.end = range.end;
                }
                // The nodes are added to the graph automatically
                IrDeclaration::Control(control) => {
                    control_idx = Some(control.to_gcl(graph, metadata).start);
                }
                IrDeclaration::Instantiation(instantiation) => {
                    commands.push(instantiation.to_gcl(graph, metadata))
                }
            }
        }

        graph.node_weight_mut(start_idx).unwrap().commands = commands;

        let main_decl = self
            .declarations
            .last()
            .and_then(|decl| match decl {
                IrDeclaration::Instantiation(instantiation) /*if instantiation.name == "main"*/ => {
                    Some(instantiation)
                }
                _ => None,
            })
            .expect("Missing main declaration");

        if !matches!(
            &main_decl.ty,
            IrType::Struct(IrStructType { id, .. })
                if metadata.type_names.get(&id).map(String::as_str) == Some("V1Switch")
        ) {
            panic!(
                "Expected type of main to be 'V1Switch', got '{:?}'",
                main_decl.ty
            );
        }

        // FIXME: This is a hard-coded way of connecting start to the control
        //        block. Instead, we should parse the main decl and use that info.
        if let Some(idx) = control_idx {
            graph.add_edge(start_idx, idx, GclPredicate::default());
        }

        start_idx

        // TODO: Parse the main decl and create driver GCL
    }
}

impl ToGcl for IrControlDecl {
    type Output = GclNodeRange;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        let mut commands = Vec::new();

        // Collect all of the top level local declarations (e.g. actions) and
        // local declarations (e.g. variables).
        for local_decl in &self.local_decls {
            match local_decl {
                IrControlLocalDecl::Variable(var_decl) => {
                    commands.push(IrStatementOrDecl::VariableDecl(var_decl.clone()));
                }
                IrControlLocalDecl::Instantiation(instantiation) => {
                    commands.push(IrStatementOrDecl::Instantiation(instantiation.clone()))
                }
                IrControlLocalDecl::Action(action_decl) => {
                    let action_range = action_decl.to_gcl(graph, metadata);

                    // Register the action under the namespace of this control block
                    graph.register_function(
                        // FIXME: Check if we actually need namespacing
                        // format!("{}::{}", self.name, action_decl.name),
                        action_decl.id,
                        action_range,
                    );
                }
                IrControlLocalDecl::Table(_table_decl) => {
                    // TODO
                }
            }
        }

        // Add in statements from the apply block
        commands.extend_from_slice(&self.apply_body.0);

        // Create the block node
        let block_range = IrBlockStatement(commands).to_gcl(graph, metadata);

        // Rename the block node start
        // TODO: replace with type id
        graph.node_weight_mut(block_range.start).unwrap().name = format!("control__{}", "todo");

        block_range
    }
}

impl ToGcl for IrActionDecl {
    type Output = GclNodeRange;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        let body_range = self.body.to_gcl(graph, metadata);
        let start_node_idx = graph.add_node(GclNode {
            name: format!("action__{}", self.id),
            // FIXME: remove ret hack
            commands: vec![GclCommand::Assignment(GclAssignment {
                name: "ret".to_string(),
                pred: GclPredicate::Bool(true),
            })],
        });
        graph.add_edge(start_node_idx, body_range.start, GclPredicate::default());

        // Note: the action is registered as a function with the graph in
        // ControlDecl::to_gcl so it can be namespaced under the control block.

        GclNodeRange {
            start: start_node_idx,
            end: body_range.end,
        }
    }
}

impl ToGcl for IrBlockStatement {
    type Output = GclNodeRange;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        let mut current_commands: Vec<GclCommand> = Vec::new();
        let mut block_start_end: Option<GclNodeRange> = None;

        fn create_node_from_commands(
            current_commands: &mut Vec<GclCommand>,
            graph: &mut GclGraph,
            block_start_end: &mut Option<GclNodeRange>,
        ) {
            // Make a node from the commands
            let name = graph.create_name("block_stmt_body");
            let node_idx = graph.add_node(GclNode {
                name,
                commands: std::mem::take(current_commands),
            });

            // Hook up the node to the end of the node chain
            if let Some(range) = block_start_end {
                graph.add_edge(range.end, node_idx, GclPredicate::default());
                *block_start_end = Some(GclNodeRange {
                    start: range.start,
                    end: node_idx,
                });
            } else {
                *block_start_end = Some(GclNodeRange {
                    start: node_idx,
                    end: node_idx,
                });
            }
        };

        // Expand each statement and collect all of the nodes (e.g. from if
        // statements) and simple commands (e.g. variable declarations).
        for statement_or_decl in &self.0 {
            match statement_or_decl.to_gcl(graph, metadata) {
                Either::Left(command) => {
                    current_commands.push(command);
                }
                Either::Right(GclNodeRange {
                    start: start_node,
                    end: end_node,
                }) => {
                    if !current_commands.is_empty() {
                        create_node_from_commands(
                            &mut current_commands,
                            graph,
                            &mut block_start_end,
                        );
                    }

                    if let Some(range) = block_start_end {
                        graph.add_edge(range.end, start_node, GclPredicate::default());
                        block_start_end = Some(GclNodeRange {
                            start: range.start,
                            end: end_node,
                        });
                    } else {
                        block_start_end = Some(GclNodeRange {
                            start: start_node,
                            end: end_node,
                        });
                    }
                }
            }
        }

        // Make sure to add any trailing commands to the graph, and ensure that
        // we have at least one node in the range.
        if !current_commands.is_empty() || block_start_end.is_none() {
            create_node_from_commands(&mut current_commands, graph, &mut block_start_end);
        }

        block_start_end.unwrap()
    }
}

impl ToGcl for IrStatementOrDecl {
    type Output = Either<GclCommand, GclNodeRange>;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        match self {
            IrStatementOrDecl::Statement(statement) => {
                Either::Right(statement.to_gcl(graph, metadata))
            }
            IrStatementOrDecl::VariableDecl(var_decl) => {
                Either::Right(var_decl.to_gcl(graph, metadata))
            }
            IrStatementOrDecl::Instantiation(instantiation) => {
                Either::Left(instantiation.to_gcl(graph, metadata))
            }
        }
    }
}

impl ToGcl for IrStatement {
    type Output = GclNodeRange;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        match self {
            IrStatement::Block(block) => block.to_gcl(graph, metadata),
            IrStatement::If(if_statement) => if_statement.to_gcl(graph, metadata),
            IrStatement::Assignment(assignment) => assignment.to_gcl(graph, metadata),
            IrStatement::FunctionCall(func_call) => func_call.to_gcl(graph, metadata),
        }
    }
}

impl ToGcl for IrVariableDecl {
    type Output = GclNodeRange;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        let mut commands = vec![GclCommand::Assignment(GclAssignment {
            name: format!("has_value__{}", self.id),
            pred: GclPredicate::Bool(self.value.is_some()),
        })];
        let name = graph.create_name(&format!("var_decl__{}", self.id));

        if !self.is_const {
            commands.push(GclCommand::Assignment(GclAssignment {
                name: format!("declared_var__{}", self.id),
                pred: GclPredicate::Bool(true),
            }));
        }

        match self.value.as_ref() {
            Some(value) => {
                let (pred, expr_range) = value.to_gcl(graph, metadata);
                commands.push(GclCommand::Assignment(GclAssignment {
                    name: format!("var__{}", self.id),
                    pred,
                }));

                let node_idx = graph.add_node(GclNode { name, commands });
                graph.add_edge(expr_range.end, node_idx, GclPredicate::default());

                GclNodeRange {
                    start: expr_range.start,
                    end: node_idx,
                }
            }
            None => {
                let node_idx = graph.add_node(GclNode { name, commands });

                GclNodeRange {
                    start: node_idx,
                    end: node_idx,
                }
            }
        }
    }
}

impl ToGcl for IrInstantiation {
    type Output = GclCommand;

    fn to_gcl(&self, _graph: &mut GclGraph, _metadata: &ProgramMetadata) -> Self::Output {
        GclCommand::Assignment(GclAssignment {
            name: format!("has_value__{}", self.id),
            pred: GclPredicate::Bool(true),
        })
    }
}

impl ToGcl for IrAssignment {
    /// The GCL node
    type Output = GclNodeRange;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        let (pred, expr_range) = self.value.to_gcl(graph, metadata);
        let node = GclNode {
            name: graph.create_name(&format!("assignment__{}", self.var)),
            commands: vec![
                GclCommand::Assignment(GclAssignment {
                    name: format!("has_value__{}", self.var),
                    pred: GclPredicate::Bool(true),
                }),
                GclCommand::Assignment(GclAssignment {
                    name: format!("var__{}", self.var),
                    pred,
                }),
            ],
        };
        let node_idx = graph.add_node(node);
        graph.add_edge(expr_range.end, node_idx, GclPredicate::default());

        let assert_node_idx = make_assert_node(
            graph,
            GclPredicate::Var(format!("declared_var__{}", self.var)),
            expr_range.start,
        );

        GclNodeRange {
            start: assert_node_idx,
            end: node_idx,
        }
    }
}

impl ToGcl for IrIfStatement {
    type Output = GclNodeRange;

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        // Create the end node first so we can refer to its index
        let end_node = GclNode {
            name: graph.create_name("if_end"),
            commands: Vec::new(),
        };
        let end_node_idx = graph.add_node(end_node);

        // Calculate the if condition predicate and nodes
        let (pred, cond_range) = self.condition.to_gcl(graph, metadata);
        let negated_pred = GclPredicate::Negation(Box::new(pred.clone()));

        // Convert the then and else branches to GCL
        let GclNodeRange {
            start: then_node_start,
            end: then_node_end,
        } = self.then_case.to_gcl(graph, metadata);
        let else_node_range = self
            .else_case
            .as_ref()
            .map(|stmt| stmt.to_gcl(graph, metadata));
        let else_node_idx = if let Some(else_range) = else_node_range {
            else_range.start
        } else {
            end_node_idx
        };

        // Connect the condition nodes to the then & else cases
        graph.add_edge(cond_range.end, then_node_start, pred);
        graph.add_edge(cond_range.end, else_node_idx, negated_pred);

        // Add edges to the end node from then and else branches
        graph.add_edge(then_node_end, end_node_idx, GclPredicate::default());
        if let Some(else_range) = else_node_range {
            graph.add_edge(else_range.end, end_node_idx, GclPredicate::default());
        }

        GclNodeRange {
            start: cond_range.start,
            end: end_node_idx,
        }
    }
}

impl ToGcl for IrExpr {
    /// The predicate which holds the boolean value of the expression, plus the
    /// nodes that calculate the predicate.
    type Output = (GclPredicate, GclNodeRange);

    fn to_gcl(&self, graph: &mut GclGraph, metadata: &ProgramMetadata) -> Self::Output {
        match &self.data {
            IrExprData::Bool(b) => {
                let (name, node_idx) = Self::single_assignment_node(graph, GclPredicate::Bool(*b));

                (
                    GclPredicate::Var(name),
                    GclNodeRange {
                        start: node_idx,
                        end: node_idx,
                    },
                )
            }
            IrExprData::Var(name) => {
                let (expr_name, node_idx) = Self::single_assignment_node(
                    graph,
                    GclPredicate::Var(format!("var__{}", name)),
                );
                let assert_idx = make_assert_node(
                    graph,
                    GclPredicate::Var(format!("has_value__{}", name)),
                    node_idx,
                );

                (
                    GclPredicate::Var(expr_name),
                    GclNodeRange {
                        start: assert_idx,
                        end: node_idx,
                    },
                )
            }
            IrExprData::And(left, right) => {
                Self::short_circuit_logic(graph, metadata, left, right, true)
            }
            IrExprData::Or(left, right) => {
                Self::short_circuit_logic(graph, metadata, left, right, false)
            }
            IrExprData::Negation(inner) => {
                let (inner_pred, inner_range) = inner.to_gcl(graph, metadata);
                let name = graph.create_name("expr");
                let node_idx = graph.add_node(GclNode {
                    name: name.clone(),
                    commands: vec![GclCommand::Assignment(GclAssignment {
                        name: name.clone(),
                        pred: GclPredicate::Negation(Box::new(inner_pred)),
                    })],
                });
                graph.add_edge(inner_range.end, node_idx, GclPredicate::default());

                (
                    GclPredicate::Var(name),
                    GclNodeRange {
                        start: inner_range.start,
                        end: node_idx,
                    },
                )
            }
            IrExprData::FunctionCall(func_call) => {
                let func_range = func_call.to_gcl(graph, metadata);
                let name = graph.create_name("expr");
                let node = GclNode {
                    name: graph.create_name("expr_func"),
                    commands: vec![GclCommand::Assignment(GclAssignment {
                        name: name.clone(),
                        pred: GclPredicate::Var("ret".to_string()),
                    })],
                };
                let node_idx = graph.add_node(node);
                graph.add_edge(func_range.end, node_idx, GclPredicate::default());

                (
                    GclPredicate::Var(name),
                    GclNodeRange {
                        start: func_range.start,
                        end: node_idx,
                    },
                )
            }
        }
    }
}

impl IrExpr {
    /// Create a node which just assigns a predicate to a variable
    fn single_assignment_node(graph: &mut GclGraph, value: GclPredicate) -> (String, NodeIndex) {
        let name = graph.create_name("expr");
        let node_idx = graph.add_node(GclNode {
            name: name.clone(),
            commands: vec![GclCommand::Assignment(GclAssignment {
                name: name.clone(),
                pred: value,
            })],
        });

        (name, node_idx)
    }

    /// The logic for short-circuiting && and || is very similar, so the
    /// implementations are generalized by this function.
    fn short_circuit_logic(
        graph: &mut GclGraph,
        metadata: &ProgramMetadata,
        left: &IrExpr,
        right: &IrExpr,
        is_add: bool,
    ) -> (GclPredicate, GclNodeRange) {
        let (left_pred, left_range) = left.to_gcl(graph, metadata);
        let (right_pred, right_range) = right.to_gcl(graph, metadata);
        let name = graph.create_name("expr");
        let op_name = if is_add { "add" } else { "or" };

        let true_node = GclNode {
            name: graph.create_name(&format!("{}_expr_true", op_name)),
            commands: vec![GclCommand::Assignment(GclAssignment {
                name: name.clone(),
                pred: GclPredicate::Bool(true),
            })],
        };
        let false_node = GclNode {
            name: graph.create_name(&format!("{}_expr_false", op_name)),
            commands: vec![GclCommand::Assignment(GclAssignment {
                name: name.clone(),
                pred: GclPredicate::Bool(false),
            })],
        };
        let end_node = GclNode {
            name: graph.create_name(&format!("{}_expr_end", op_name)),
            commands: Vec::new(),
        };

        let true_node_idx = graph.add_node(true_node);
        let false_node_idx = graph.add_node(false_node);
        let end_node_idx = graph.add_node(end_node);
        let left_pred_negated = GclPredicate::Negation(Box::new(left_pred.clone()));
        let right_pred_negated = GclPredicate::Negation(Box::new(right_pred.clone()));

        let (left_to_right, left_to_set) = if is_add {
            (left_pred, left_pred_negated)
        } else {
            (left_pred_negated, left_pred)
        };

        graph.add_edge(left_range.end, right_range.start, left_to_right);
        graph.add_edge(
            left_range.end,
            if is_add {
                false_node_idx
            } else {
                true_node_idx
            },
            left_to_set,
        );
        graph.add_edge(right_range.end, true_node_idx, right_pred);
        graph.add_edge(right_range.end, false_node_idx, right_pred_negated);
        graph.add_edge(true_node_idx, end_node_idx, GclPredicate::default());
        graph.add_edge(false_node_idx, end_node_idx, GclPredicate::default());

        (
            GclPredicate::Var(name),
            GclNodeRange {
                start: left_range.start,
                end: end_node_idx,
            },
        )
    }
}

impl ToGcl for IrFunctionCall {
    type Output = GclNodeRange;

    fn to_gcl(&self, graph: &mut GclGraph, _metadata: &ProgramMetadata) -> Self::Output {
        // TODO: handle setting arguments and verifying args have values
        let function_range = graph
            .get_function(self.target)
            .unwrap_or_else(|| panic!("Unable to find function {}", self.target));

        let start_name = graph.create_name("func_call_start");
        let end_name = graph.create_name("func_call_end");
        let ret_target_var = format!("func_ret_target__{}", self.target);
        let start_idx = graph.add_node(GclNode {
            name: start_name,
            commands: vec![GclCommand::Assignment(GclAssignment {
                name: ret_target_var.clone(),
                pred: GclPredicate::String(end_name.clone()),
            })],
        });
        let end_idx = graph.add_node(GclNode {
            name: end_name.clone(),
            commands: Vec::new(),
        });

        graph.add_edge(start_idx, function_range.start, GclPredicate::default());
        graph.add_edge(
            function_range.end,
            end_idx,
            GclPredicate::Equality(
                Box::new(GclPredicate::StringVar(ret_target_var)),
                Box::new(GclPredicate::String(end_name)),
            ),
        );

        GclNodeRange {
            start: start_idx,
            end: end_idx,
        }
    }
}

/// Create an assertion node which, when the predicate is true, jumps to
/// the `next_node`, otherwise jumps to a new "bug" node.
fn make_assert_node(
    graph: &mut GclGraph,
    predicate: GclPredicate,
    next_node: NodeIndex,
) -> NodeIndex {
    let bug_node = GclNode {
        name: graph.create_name("bug"),
        commands: vec![GclCommand::Bug],
    };
    let bug_node_idx = graph.add_node(bug_node);

    let assert_node = GclNode {
        name: graph.create_name("assert"),
        commands: Vec::new(),
    };
    let assert_node_idx = graph.add_node(assert_node);

    graph.add_edge(assert_node_idx, next_node, predicate.clone());
    graph.add_edge(
        assert_node_idx,
        bug_node_idx,
        GclPredicate::Negation(Box::new(predicate)),
    );

    assert_node_idx
}
