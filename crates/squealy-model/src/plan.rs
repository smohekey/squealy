//! Deployment planning over schema model diffs.
//!
//! A plan is the ordered, policy-checked form of a [`DatabaseDiff`]. It is still backend-neutral:
//! backend crates remain responsible for deciding whether and how individual steps can be rendered.

use crate::{
    ChangeRisk, DatabaseDiff, DatabaseDiffChange, DatabaseModel, DiffPolicy, DiffPolicyError,
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
