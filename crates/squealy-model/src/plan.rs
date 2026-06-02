//! Deployment planning over schema model diffs.
//!
//! A plan is the ordered, policy-checked form of a [`DatabaseDiff`]. It is still backend-neutral:
//! backend crates remain responsible for deciding whether and how individual steps can be rendered.

use crate::{
    ChangeRisk, DatabaseDiff, DatabaseDiffChange, DatabaseModel, DiffPolicy, DiffPolicyError,
    TableDiffChange, check_diff_policy, diff_models,
};
use squealy::TableModel;

/// An ordered deployment plan from an actual model to a desired model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DatabasePlan {
    pub steps: Vec<DatabasePlanStep>,
}

impl DatabasePlan {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn classified_steps(&self) -> Vec<ClassifiedDatabasePlanStep> {
        self.steps
            .iter()
            .map(|step| ClassifiedDatabasePlanStep {
                risk: step.risk(),
                step: step.clone(),
            })
            .collect()
    }
}

/// A plan step with conservative deployment-risk classification.
#[derive(Clone, Debug, PartialEq)]
pub struct ClassifiedDatabasePlanStep {
    pub risk: ChangeRisk,
    pub step: DatabasePlanStep,
}

/// One ordered backend-neutral deployment step.
#[derive(Clone, Debug, PartialEq)]
pub enum DatabasePlanStep {
    CreateSchema {
        schema: Option<String>,
    },
    DropSchema {
        schema: Option<String>,
    },
    CreateTable {
        schema: Option<String>,
        table: TableModel,
    },
    DropTable {
        schema: Option<String>,
        table: TableModel,
    },
    AlterTable {
        schema: Option<String>,
        table: String,
        change: TableDiffChange,
    },
}

impl DatabasePlanStep {
    pub fn risk(&self) -> ChangeRisk {
        match self {
            DatabasePlanStep::CreateSchema { .. } | DatabasePlanStep::CreateTable { .. } => {
                ChangeRisk::Safe
            }
            DatabasePlanStep::DropSchema { .. } | DatabasePlanStep::DropTable { .. } => {
                ChangeRisk::Destructive
            }
            DatabasePlanStep::AlterTable { change, .. } => change.risk(),
        }
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
                        change: table_change.clone(),
                    });
                }
            }
        }
    }
    steps
}
