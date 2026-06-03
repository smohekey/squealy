//! Deployment planning over schema model diffs.
//!
//! A plan is the ordered, policy-checked form of a [`DatabaseDiff`]. It is still backend-neutral:
//! backend crates remain responsible for deciding whether and how individual steps can be rendered.

use crate::diff::diff_table;
use crate::{
    ChangeRisk, ClassifiedDatabaseDiffChange, DatabaseDiff, DatabaseDiffChange, DatabaseModel,
    DiffPolicy, DiffPolicyError, RefactorLog, RefactorOperation, RenameColumn, RenameTable,
    TableDiffChange, check_diff_policy, diff_models,
};
use squealy::{DatabasePlan, DatabasePlanStep, TablePlanStep};

/// A plan step with conservative deployment-risk classification.
#[derive(Clone, Debug, PartialEq)]
pub struct ClassifiedDatabasePlanStep {
    pub risk: ChangeRisk,
    pub step: DatabasePlanStep,
}

/// Classifies every step in `plan`.
pub fn classified_plan_steps(plan: &DatabasePlan) -> Vec<ClassifiedDatabasePlanStep> {
    plan.steps
        .iter()
        .map(|step| ClassifiedDatabasePlanStep {
            risk: plan_step_risk(step),
            step: step.clone(),
        })
        .collect()
}

/// Returns the conservative deployment-risk classification for `step`.
pub fn plan_step_risk(step: &DatabasePlanStep) -> ChangeRisk {
    match step {
        DatabasePlanStep::CreateSchema { .. } | DatabasePlanStep::CreateTable { .. } => {
            ChangeRisk::Safe
        }
        DatabasePlanStep::RenameTable { .. } => ChangeRisk::Safe,
        DatabasePlanStep::DropSchema { .. } | DatabasePlanStep::DropTable { .. } => {
            ChangeRisk::Destructive
        }
        DatabasePlanStep::AlterTable { change, .. } => table_plan_step_risk(change),
    }
}

/// Returns the conservative deployment-risk classification for `step`.
pub fn table_plan_step_risk(step: &TablePlanStep) -> ChangeRisk {
    match step {
        TablePlanStep::SetTableComment { .. }
        | TablePlanStep::RenameColumn { .. }
        | TablePlanStep::AddPrimaryKey { .. }
        | TablePlanStep::AddUnique { .. }
        | TablePlanStep::AddForeignKey { .. }
        | TablePlanStep::AddCheck { .. }
        | TablePlanStep::AddIndex { .. } => ChangeRisk::Safe,
        TablePlanStep::DropColumn { .. }
        | TablePlanStep::DropPrimaryKey { .. }
        | TablePlanStep::DropUnique { .. }
        | TablePlanStep::DropForeignKey { .. }
        | TablePlanStep::DropCheck { .. }
        | TablePlanStep::DropIndex { .. } => ChangeRisk::Destructive,
        TablePlanStep::AddColumn { column } => {
            if column.nullable || column.default.is_some() || column.identity.is_some() {
                ChangeRisk::Safe
            } else {
                ChangeRisk::Ambiguous
            }
        }
        TablePlanStep::AlterColumn { .. }
        | TablePlanStep::AlterPrimaryKey { .. }
        | TablePlanStep::AlterUnique { .. }
        | TablePlanStep::AlterForeignKey { .. }
        | TablePlanStep::AlterCheck { .. }
        | TablePlanStep::AlterIndex { .. } => ChangeRisk::Ambiguous,
    }
}

/// Builds a policy-checked plan from a precomputed diff.
pub fn plan_diff(diff: &DatabaseDiff, policy: DiffPolicy) -> Result<DatabasePlan, DiffPolicyError> {
    check_diff_policy(diff, policy)?;
    Ok(DatabasePlan {
        steps: flatten_diff(diff),
    })
}

/// Diffs `desired` against `actual`, then builds a policy-checked plan.
pub fn plan_models(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
    policy: DiffPolicy,
) -> Result<DatabasePlan, DiffPolicyError> {
    plan_diff(&diff_models(desired, actual), policy)
}

/// Diffs `desired` against `actual`, applies explicit refactor intent, then builds a policy-checked
/// plan.
///
/// Refactors are applied before policy checking, so an explicit rename is treated as safe instead of
/// the destructive drop/add pair that a name-based diff would otherwise produce.
pub fn plan_models_with_refactors(
    desired: &DatabaseModel,
    actual: &DatabaseModel,
    refactors: &RefactorLog,
    policy: DiffPolicy,
) -> Result<DatabasePlan, DiffPolicyError> {
    let mut steps = flatten_diff(&diff_models(desired, actual));
    apply_refactors(&mut steps, refactors);
    check_plan_policy(&steps, policy)?;
    Ok(DatabasePlan { steps })
}

fn check_plan_policy(
    steps: &[DatabasePlanStep],
    policy: DiffPolicy,
) -> Result<(), DiffPolicyError> {
    let blocked = steps
        .iter()
        .filter_map(|step| {
            let risk = plan_step_risk(step);
            (!policy.allows(risk)).then(|| ClassifiedDatabaseDiffChange {
                risk,
                change: plan_step_as_diff_change(step),
            })
        })
        .collect::<Vec<_>>();

    if blocked.is_empty() {
        Ok(())
    } else {
        Err(DiffPolicyError { blocked })
    }
}

fn plan_step_as_diff_change(step: &DatabasePlanStep) -> DatabaseDiffChange {
    match step {
        DatabasePlanStep::CreateSchema { schema } => DatabaseDiffChange::CreateSchema {
            schema: schema.clone(),
        },
        DatabasePlanStep::DropSchema { schema } => DatabaseDiffChange::DropSchema {
            schema: schema.clone(),
        },
        DatabasePlanStep::CreateTable { schema, table } => DatabaseDiffChange::CreateTable {
            schema: schema.clone(),
            table: table.clone(),
        },
        DatabasePlanStep::DropTable { schema, table } => DatabaseDiffChange::DropTable {
            schema: schema.clone(),
            table: table.clone(),
        },
        DatabasePlanStep::RenameTable { schema, from, to } => DatabaseDiffChange::AlterTable {
            schema: schema.clone(),
            table: from.clone(),
            changes: vec![TableDiffChange::SetTableComment {
                before: Some(format!("rename table from {from}")),
                after: Some(format!("rename table to {to}")),
            }],
        },
        DatabasePlanStep::AlterTable {
            schema,
            table,
            change,
        } => DatabaseDiffChange::AlterTable {
            schema: schema.clone(),
            table: table.clone(),
            changes: vec![table_plan_step_as_diff_change(change)],
        },
    }
}

fn table_plan_step_as_diff_change(step: &TablePlanStep) -> TableDiffChange {
    match step {
        TablePlanStep::SetTableComment { before, after } => TableDiffChange::SetTableComment {
            before: before.clone(),
            after: after.clone(),
        },
        TablePlanStep::AddColumn { column } => TableDiffChange::AddColumn {
            column: column.clone(),
        },
        TablePlanStep::DropColumn { column } => TableDiffChange::DropColumn {
            column: column.clone(),
        },
        TablePlanStep::RenameColumn { from, to } => TableDiffChange::SetTableComment {
            before: Some(format!("rename column from {from}")),
            after: Some(format!("rename column to {to}")),
        },
        TablePlanStep::AlterColumn { before, after } => TableDiffChange::AlterColumn {
            before: before.clone(),
            after: after.clone(),
        },
        TablePlanStep::AddPrimaryKey { constraint } => TableDiffChange::AddPrimaryKey {
            constraint: constraint.clone(),
        },
        TablePlanStep::DropPrimaryKey { constraint } => TableDiffChange::DropPrimaryKey {
            constraint: constraint.clone(),
        },
        TablePlanStep::AlterPrimaryKey { before, after } => TableDiffChange::AlterPrimaryKey {
            before: before.clone(),
            after: after.clone(),
        },
        TablePlanStep::AddUnique { constraint } => TableDiffChange::AddUnique {
            constraint: constraint.clone(),
        },
        TablePlanStep::DropUnique { constraint } => TableDiffChange::DropUnique {
            constraint: constraint.clone(),
        },
        TablePlanStep::AlterUnique { before, after } => TableDiffChange::AlterUnique {
            before: before.clone(),
            after: after.clone(),
        },
        TablePlanStep::AddForeignKey { foreign_key } => TableDiffChange::AddForeignKey {
            foreign_key: foreign_key.clone(),
        },
        TablePlanStep::DropForeignKey { foreign_key } => TableDiffChange::DropForeignKey {
            foreign_key: foreign_key.clone(),
        },
        TablePlanStep::AlterForeignKey { before, after } => TableDiffChange::AlterForeignKey {
            before: before.clone(),
            after: after.clone(),
        },
        TablePlanStep::AddCheck { check } => TableDiffChange::AddCheck {
            check: check.clone(),
        },
        TablePlanStep::DropCheck { check } => TableDiffChange::DropCheck {
            check: check.clone(),
        },
        TablePlanStep::AlterCheck { before, after } => TableDiffChange::AlterCheck {
            before: before.clone(),
            after: after.clone(),
        },
        TablePlanStep::AddIndex { index } => TableDiffChange::AddIndex {
            index: index.clone(),
        },
        TablePlanStep::DropIndex { index } => TableDiffChange::DropIndex {
            index: index.clone(),
        },
        TablePlanStep::AlterIndex { before, after } => TableDiffChange::AlterIndex {
            before: before.clone(),
            after: after.clone(),
        },
    }
}

fn flatten_diff(diff: &DatabaseDiff) -> Vec<DatabasePlanStep> {
    let mut steps = Vec::new();
    for change in &diff.changes {
        match change {
            DatabaseDiffChange::CreateSchema { schema } => {
                steps.push(DatabasePlanStep::CreateSchema {
                    schema: schema.clone(),
                });
            }
            DatabaseDiffChange::DropSchema { schema } => {
                steps.push(DatabasePlanStep::DropSchema {
                    schema: schema.clone(),
                });
            }
            DatabaseDiffChange::CreateTable { schema, table } => {
                steps.push(DatabasePlanStep::CreateTable {
                    schema: schema.clone(),
                    table: table.clone(),
                });
            }
            DatabaseDiffChange::DropTable { schema, table } => {
                steps.push(DatabasePlanStep::DropTable {
                    schema: schema.clone(),
                    table: table.clone(),
                });
            }
            DatabaseDiffChange::AlterTable {
                schema,
                table,
                changes,
            } => {
                for table_change in changes {
                    steps.push(DatabasePlanStep::AlterTable {
                        schema: schema.clone(),
                        table: table.clone(),
                        change: table_plan_step(table_change),
                    });
                }
            }
        }
    }
    steps
}

fn apply_refactors(steps: &mut Vec<DatabasePlanStep>, refactors: &RefactorLog) {
    for operation in &refactors.operations {
        match operation {
            RefactorOperation::RenameTable(operation) => {
                apply_table_rename(steps, operation);
            }
            RefactorOperation::RenameColumn(operation) => {
                apply_column_rename(steps, operation);
            }
        }
    }
}

fn apply_table_rename(steps: &mut Vec<DatabasePlanStep>, operation: &RenameTable) {
    let drop_match = steps
        .iter()
        .enumerate()
        .find_map(|(position, step)| match step {
            DatabasePlanStep::DropTable { schema, table }
                if schema == &operation.schema && table.name == operation.from =>
            {
                Some((position, table.clone()))
            }
            _ => None,
        });
    let create_match = steps
        .iter()
        .enumerate()
        .find_map(|(position, step)| match step {
            DatabasePlanStep::CreateTable { schema, table }
                if schema == &operation.schema && table.name == operation.to =>
            {
                Some((position, table.clone()))
            }
            _ => None,
        });
    let (Some((drop_position, mut actual_table)), Some((create_position, desired_table))) =
        (drop_match, create_match)
    else {
        return;
    };

    let insert_position = drop_position.min(create_position);
    remove_positions(steps, drop_position, create_position);
    actual_table.name = operation.to.clone();
    let follow_up_changes = diff_table(&desired_table, &actual_table);
    let mut replacement = vec![DatabasePlanStep::RenameTable {
        schema: operation.schema.clone(),
        from: operation.from.clone(),
        to: operation.to.clone(),
    }];
    replacement.extend(
        follow_up_changes
            .into_iter()
            .map(|change| DatabasePlanStep::AlterTable {
                schema: operation.schema.clone(),
                table: operation.to.clone(),
                change: table_plan_step(&change),
            }),
    );
    steps.insert(insert_position, replacement.remove(0));
    for (offset, step) in replacement.into_iter().enumerate() {
        steps.insert(insert_position + 1 + offset, step);
    }
}

fn apply_column_rename(steps: &mut Vec<DatabasePlanStep>, operation: &RenameColumn) {
    let drop_match = steps
        .iter()
        .enumerate()
        .find_map(|(position, step)| match step {
            DatabasePlanStep::AlterTable {
                schema,
                table,
                change: TablePlanStep::DropColumn { column },
            } if schema == &operation.schema
                && table == &operation.table
                && column.name == operation.from =>
            {
                Some((position, column.clone()))
            }
            _ => None,
        });
    let add_match = steps
        .iter()
        .enumerate()
        .find_map(|(position, step)| match step {
            DatabasePlanStep::AlterTable {
                schema,
                table,
                change: TablePlanStep::AddColumn { column },
            } if schema == &operation.schema
                && table == &operation.table
                && column.name == operation.to =>
            {
                Some((position, column.clone()))
            }
            _ => None,
        });
    let (Some((drop_position, mut before_column)), Some((add_position, after_column))) =
        (drop_match, add_match)
    else {
        return;
    };

    let insert_position = drop_position.min(add_position);
    remove_positions(steps, drop_position, add_position);
    before_column.name = operation.to.clone();
    let mut replacement = vec![DatabasePlanStep::AlterTable {
        schema: operation.schema.clone(),
        table: operation.table.clone(),
        change: TablePlanStep::RenameColumn {
            from: operation.from.clone(),
            to: operation.to.clone(),
        },
    }];
    if before_column != after_column {
        replacement.push(DatabasePlanStep::AlterTable {
            schema: operation.schema.clone(),
            table: operation.table.clone(),
            change: TablePlanStep::AlterColumn {
                before: before_column,
                after: after_column,
            },
        });
    }
    for (offset, step) in replacement.into_iter().enumerate() {
        steps.insert(insert_position + offset, step);
    }
}

fn remove_positions<T>(items: &mut Vec<T>, left: usize, right: usize) {
    if left > right {
        items.remove(left);
        items.remove(right);
    } else {
        items.remove(right);
        items.remove(left);
    }
}

fn table_plan_step(change: &TableDiffChange) -> TablePlanStep {
    match change {
        TableDiffChange::SetTableComment { before, after } => TablePlanStep::SetTableComment {
            before: before.clone(),
            after: after.clone(),
        },
        TableDiffChange::AddColumn { column } => TablePlanStep::AddColumn {
            column: column.clone(),
        },
        TableDiffChange::DropColumn { column } => TablePlanStep::DropColumn {
            column: column.clone(),
        },
        TableDiffChange::AlterColumn { before, after } => TablePlanStep::AlterColumn {
            before: before.clone(),
            after: after.clone(),
        },
        TableDiffChange::AddPrimaryKey { constraint } => TablePlanStep::AddPrimaryKey {
            constraint: constraint.clone(),
        },
        TableDiffChange::DropPrimaryKey { constraint } => TablePlanStep::DropPrimaryKey {
            constraint: constraint.clone(),
        },
        TableDiffChange::AlterPrimaryKey { before, after } => TablePlanStep::AlterPrimaryKey {
            before: before.clone(),
            after: after.clone(),
        },
        TableDiffChange::AddUnique { constraint } => TablePlanStep::AddUnique {
            constraint: constraint.clone(),
        },
        TableDiffChange::DropUnique { constraint } => TablePlanStep::DropUnique {
            constraint: constraint.clone(),
        },
        TableDiffChange::AlterUnique { before, after } => TablePlanStep::AlterUnique {
            before: before.clone(),
            after: after.clone(),
        },
        TableDiffChange::AddForeignKey { foreign_key } => TablePlanStep::AddForeignKey {
            foreign_key: foreign_key.clone(),
        },
        TableDiffChange::DropForeignKey { foreign_key } => TablePlanStep::DropForeignKey {
            foreign_key: foreign_key.clone(),
        },
        TableDiffChange::AlterForeignKey { before, after } => TablePlanStep::AlterForeignKey {
            before: before.clone(),
            after: after.clone(),
        },
        TableDiffChange::AddCheck { check } => TablePlanStep::AddCheck {
            check: check.clone(),
        },
        TableDiffChange::DropCheck { check } => TablePlanStep::DropCheck {
            check: check.clone(),
        },
        TableDiffChange::AlterCheck { before, after } => TablePlanStep::AlterCheck {
            before: before.clone(),
            after: after.clone(),
        },
        TableDiffChange::AddIndex { index } => TablePlanStep::AddIndex {
            index: index.clone(),
        },
        TableDiffChange::DropIndex { index } => TablePlanStep::DropIndex {
            index: index.clone(),
        },
        TableDiffChange::AlterIndex { before, after } => TablePlanStep::AlterIndex {
            before: before.clone(),
            after: after.clone(),
        },
    }
}
