//! Canonical "model" backend and the view-body lowering it drives.
//!
//! A view body is a `SELECT` that must become a backend-neutral [`ViewQueryModel`] whose expressions
//! are stored **structurally** ([`ExprNode`]) so each backend renders them in its own dialect, with
//! literals **inlined** (a view definition carries no bind parameters). The query builder only lets a
//! literal out through [`Encode`], and only lowers a query through a [`SelectSink`] bound to a
//! concrete [`Backend`]. So this module defines a render-only [`ModelBackend`]/[`ModelConn`] whose
//! `Encode` impls format SQL literals, and a [`ModelSink`] + [`IrBuilder`] that walk the typed AST's
//! structural visitors into an [`ExprNode`] tree. None of this ever executes — it exists purely to
//! turn a typed definition into the neutral model that backends render `CREATE VIEW` from.

use std::borrow::Cow;
use std::io::{self, Write};
use std::marker::PhantomData;

use crate::{
    AggregateFunc, ArithmeticOp, Backend, CaseArm, ColumnRef, CompareOp, DateField, Decode, Encode,
    Expr, ExprKind, ExprNode, ExprVisitor, InsertableTable, JoinItem, JoinKind, LogicalOp, Order,
    OrderDirection, OrderItem, ParamWriter, Predicate, PredicateAstVisitor, PredicateKind,
    Projectable, ProjectionItem, ProjectionShape, ProjectionVisitor, QueryBuilder, RenderAst,
    RenderCaseArms, RenderCoalesceArgs, RenderPredicateAst, RenderProjectable, RenderSelectAst,
    RenderSimpleCaseArms, RenderSubquery, RowReader, ScalarFunc, SelectAst, SelectSink, Selected,
    SourceAlias, SourceRef, SqlType, Table, TableProjection, UnaryStringFunc, ViewQueryModel,
    WindowFunc, WindowOrderTerm,
};

// ---------------------------------------------------------------------------
// Canonical backend
// ---------------------------------------------------------------------------

/// A render-only backend whose native parameter is the SQL-literal text of a value. It never connects
/// or executes; it exists so the shared renderer can inline literals into a view body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelBackend;

/// The render-only connection that view definitions are built against. Constructing the query AST
/// requires a [`QueryBuilder`]; this one only provides the type machinery, never execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelConn;

/// The (unconstructed) error type for [`ModelBackend`]. Rendering a literal to text cannot fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelError;

/// Collects rendered SQL-literal text. [`ModelBackend::Param`] is the literal string itself.
pub struct ModelParamWriter<'params> {
    params: &'params mut Vec<String>,
}

impl ModelParamWriter<'_> {
    fn push_literal(&mut self, literal: String) {
        self.params.push(literal);
    }
}

impl ParamWriter for ModelParamWriter<'_> {
    type Backend = ModelBackend;

    fn write<T>(&mut self, value: &T) -> Result<(), ModelError>
    where
        T: Encode<ModelBackend>,
    {
        value.encode(self)
    }
}

/// The (unconstructed) row reader for [`ModelBackend`]; the model backend never reads rows.
pub struct ModelRowReader<'row>(PhantomData<&'row ()>, ModelNever);

/// An uninhabited type that makes [`ModelRowReader`] impossible to construct.
enum ModelNever {}

impl RowReader for ModelRowReader<'_> {
    type Backend = ModelBackend;

    fn read<T>(&mut self) -> Result<T, ModelError>
    where
        T: Decode<ModelBackend>,
    {
        match self.1 {}
    }
}

impl Backend for ModelBackend {
    type Error = ModelError;
    type RowReader<'row> = ModelRowReader<'row>;
    type ParamWriter<'param> = ModelParamWriter<'param>;
    type Param = String;

    fn param_writer(params: &mut Vec<Self::Param>) -> Self::ParamWriter<'_> {
        ModelParamWriter { params }
    }

    fn no_rows_error() -> Self::Error {
        ModelError
    }

    fn write_table(&self, _table: &(dyn Table + Sync), _writer: &mut impl Write) -> io::Result<()> {
        // The model backend renders views, not tables; table DDL goes through the real backends.
        unreachable!("ModelBackend does not render table DDL")
    }
}

// View bodies are dialect-neutral, so the model backend allows `full_join` (a full-join view is valid
// against PostgreSQL; deploying it to MySQL — which has no `FULL JOIN` — fails at DDL exec, as noted on
// `full_join`).
impl crate::SupportsFullJoin for ModelBackend {}
// Likewise `date_trunc`: a view carrying it is lowered against the model backend; rendering to MySQL
// (which has no `date_trunc`) fails at DDL exec, as with a `full_join` view.
impl crate::SupportsDateTrunc for ModelBackend {}

// ---------------------------------------------------------------------------
// Literal encoding: every value becomes its SQL-literal text
// ---------------------------------------------------------------------------

macro_rules! encode_display {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Encode<ModelBackend> for $ty {
                fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
                    out.push_literal(self.to_string());
                    Ok(())
                }
            }
        )*
    };
}

encode_display!(
    i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64
);

impl Encode<ModelBackend> for bool {
    fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
        out.push_literal(if *self { "TRUE" } else { "FALSE" }.to_owned());
        Ok(())
    }
}

impl Encode<ModelBackend> for str {
    fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
        out.push_literal(format!("'{}'", self.replace('\'', "''")));
        Ok(())
    }
}

impl Encode<ModelBackend> for String {
    fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
        self.as_str().encode(out)
    }
}

impl<T> Encode<ModelBackend> for Option<T>
where
    T: Encode<ModelBackend>,
{
    fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
        match self {
            Some(value) => value.encode(out),
            None => {
                out.push_literal("NULL".to_owned());
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Expression IR builder
// ---------------------------------------------------------------------------

/// Encodes a literal value to inlined SQL text via [`ModelBackend`]'s `Encode` impls.
fn encode_literal<T>(value: &T) -> String
where
    T: Encode<ModelBackend>,
{
    let mut params = Vec::new();
    {
        let mut writer = ModelBackend::param_writer(&mut params);
        // `Encode` for `ModelBackend` is infallible.
        let _ = value.encode(&mut writer);
    }
    params.concat()
}

/// Builds a neutral [`ExprNode`] tree from the typed expression/predicate AST with a node stack: each
/// leaf pushes a node and each combinator runs its child closures (which push their nodes) then pops
/// and combines. Driven by the structural [`ExprVisitor`]/[`PredicateAstVisitor`] visits.
#[derive(Default)]
struct IrBuilder {
    stack: Vec<ExprNode>,
    /// Window `ORDER BY` directions, recorded by `visit_window_order_direction` and paired with the
    /// order expressions in `visit_window`.
    window_order_directions: Vec<OrderDirection>,
}

impl IrBuilder {
    fn pop(&mut self) -> Box<ExprNode> {
        Box::new(self.stack.pop().expect("expression IR stack underflow"))
    }

    /// Runs a child visit closure and returns the node it pushed.
    fn child<F>(&mut self, child: F) -> io::Result<Box<ExprNode>>
    where
        F: FnOnce(&mut Self) -> io::Result<()>,
    {
        child(self)?;
        Ok(self.pop())
    }

    fn finish(mut self) -> ExprNode {
        self.stack.pop().expect("expression IR produced no node")
    }
}

impl ExprVisitor for IrBuilder {
    type Error = io::Error;
    type Backend = ModelBackend;

    fn visit_column(&mut self, alias: SourceAlias, column: &str) -> io::Result<()> {
        self.stack.push(ExprNode::Column {
            alias: alias.to_string(),
            column: column.to_owned(),
        });
        Ok(())
    }

    fn visit_literal<T>(&mut self, value: &T) -> io::Result<()>
    where
        T: Encode<ModelBackend>,
    {
        self.stack.push(ExprNode::Literal(encode_literal(value)));
        Ok(())
    }

    fn visit_param(&mut self) -> io::Result<()> {
        // `ViewSelect`'s `NoRuntimeParams` bound rejects parameterized definitions upstream, so a
        // runtime parameter never reaches the lowering.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a view body cannot contain a runtime parameter",
        ))
    }

    fn visit_binary<L, R>(&mut self, op: ArithmeticOp, left: L, right: R) -> io::Result<()>
    where
        L: FnOnce(&mut Self) -> io::Result<()>,
        R: FnOnce(&mut Self) -> io::Result<()>,
    {
        let left = self.child(left)?;
        let right = self.child(right)?;
        self.stack.push(ExprNode::Binary { op, left, right });
        Ok(())
    }

    fn visit_nullif<L, R>(
        &mut self,
        left: L,
        _left_needs_cast: bool,
        right: R,
        _right_needs_cast: bool,
        result: Option<&SqlType>,
    ) -> io::Result<()>
    where
        L: FnOnce(&mut Self) -> io::Result<()>,
        R: FnOnce(&mut Self) -> io::Result<()>,
    {
        // View bodies inline literals (which are typed), so the per-operand cast decision is made
        // structurally by the view renderer (cast only inlined literals, never columns); `result` is
        // captured for that.
        let left = self.child(left)?;
        let right = self.child(right)?;
        self.stack.push(ExprNode::Nullif {
            left,
            right,
            result: result.cloned(),
        });
        Ok(())
    }

    fn visit_coalesce<Args>(
        &mut self,
        args: &Args,
        _all_args_need_cast: bool,
        result: Option<&SqlType>,
    ) -> io::Result<()>
    where
        Args: RenderCoalesceArgs<ModelBackend>,
    {
        // Each argument pushes one node (the separator/cast hooks are no-ops here; the cast is captured
        // in `result` and applied per argument by the view renderer); split them off the stack.
        let args_start = self.stack.len();
        args.render(self, result, true)?;
        let nodes = self.stack.split_off(args_start);
        self.stack.push(ExprNode::Coalesce {
            args: nodes,
            result: result.cloned(),
        });
        Ok(())
    }

    fn visit_coalesce_separator(&mut self) -> io::Result<()> {
        // Arguments are already distinct nodes on the stack.
        Ok(())
    }

    fn visit_aggregate<O>(
        &mut self,
        func: AggregateFunc,
        distinct: bool,
        cast: Option<&SqlType>,
        operand: O,
    ) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
    {
        let operand = self.child(operand)?;
        self.stack.push(ExprNode::Aggregate {
            func,
            distinct,
            operand,
            result: cast.cloned(),
        });
        Ok(())
    }

    fn visit_scalar_subquery<Sub>(&mut self, subquery: &Sub) -> io::Result<()>
    where
        Sub: RenderSubquery<ModelBackend>,
    {
        self.stack
            .push(ExprNode::ScalarSubquery(Box::new(lower_subquery(
                subquery,
            )?)));
        Ok(())
    }

    fn visit_window<Operand, Partitions, Orders>(
        &mut self,
        func: WindowFunc,
        cast: Option<&SqlType>,
        operand: Operand,
        has_partitions: bool,
        partitions: Partitions,
        has_orders: bool,
        orders: Orders,
    ) -> io::Result<()>
    where
        Operand: FnOnce(&mut Self) -> io::Result<()>,
        Partitions: FnOnce(&mut Self) -> io::Result<()>,
        Orders: FnOnce(&mut Self) -> io::Result<()>,
    {
        // Each list closure pushes its (variable number of) element nodes; capture them by splitting
        // the stack at the depth recorded before the closure ran. Window order directions are recorded
        // in parallel by `visit_window_order_direction` and zipped with the order expressions.
        let args_start = self.stack.len();
        operand(self)?;
        let args = self.stack.split_off(args_start);

        let partitions_start = self.stack.len();
        if has_partitions {
            partitions(self)?;
        }
        let partition_by = self.stack.split_off(partitions_start);

        let directions_start = self.window_order_directions.len();
        let orders_start = self.stack.len();
        if has_orders {
            orders(self)?;
        }
        let order_exprs = self.stack.split_off(orders_start);
        let directions = self.window_order_directions.split_off(directions_start);
        let order_by = order_exprs
            .into_iter()
            .zip(directions)
            .map(|(expr, direction)| WindowOrderTerm { expr, direction })
            .collect();

        self.stack.push(ExprNode::Window {
            func,
            args,
            partition_by,
            order_by,
            result: cast.cloned(),
        });
        Ok(())
    }

    fn visit_window_separator(&mut self) -> io::Result<()> {
        // List elements are already separated as distinct nodes on the stack.
        Ok(())
    }

    fn visit_window_order_direction(&mut self, direction: OrderDirection) -> io::Result<()> {
        self.window_order_directions.push(direction);
        Ok(())
    }

    fn visit_case<Arms, Else>(
        &mut self,
        arms: &Arms,
        else_: Option<&Else>,
        result: Option<&SqlType>,
    ) -> io::Result<()>
    where
        Arms: RenderCaseArms<ModelBackend>,
        Else: RenderAst<ModelBackend>,
    {
        // Each arm pushes its predicate node then its value node (the keyword/cast hooks are no-ops
        // here; the cast is captured in `result` and applied per branch by the view renderer), so the
        // stack grows by `2 * LEN`; split it off and pair the nodes back up.
        let arms_start = self.stack.len();
        arms.render(self, result)?;
        let mut nodes = self.stack.split_off(arms_start).into_iter();
        let mut case_arms = Vec::with_capacity(Arms::LEN);
        for _ in 0..Arms::LEN {
            let when = Box::new(nodes.next().expect("CASE arm predicate node"));
            let then = Box::new(nodes.next().expect("CASE arm value node"));
            case_arms.push(CaseArm { when, then });
        }
        let else_ = match else_ {
            Some(else_) => Some(self.child(|builder| else_.visit(builder))?),
            None => None,
        };
        self.stack.push(ExprNode::Case {
            arms: case_arms,
            else_,
            result: result.cloned(),
        });
        Ok(())
    }

    fn visit_simple_case<Operand, Arms, Else>(
        &mut self,
        operand: Operand,
        _operand_needs_cast: bool,
        _cmp: Option<&SqlType>,
        arms: &Arms,
        else_: Option<&Else>,
        result: Option<&SqlType>,
    ) -> io::Result<()>
    where
        Operand: FnOnce(&mut Self) -> io::Result<()>,
        Arms: RenderSimpleCaseArms<ModelBackend>,
        Else: RenderAst<ModelBackend>,
    {
        // View bodies inline the operand (a typed literal or a column), so it needs no cast anchor.
        let operand = self.child(operand)?;
        // Each arm pushes its WHEN-value node then its THEN-value node (2 * LEN nodes).
        let arms_start = self.stack.len();
        arms.render(self, result)?;
        let mut nodes = self.stack.split_off(arms_start).into_iter();
        let mut case_arms = Vec::with_capacity(Arms::LEN);
        for _ in 0..Arms::LEN {
            let when = Box::new(nodes.next().expect("simple CASE WHEN node"));
            let then = Box::new(nodes.next().expect("simple CASE THEN node"));
            case_arms.push(CaseArm { when, then });
        }
        let else_ = match else_ {
            Some(else_) => Some(self.child(|builder| else_.visit(builder))?),
            None => None,
        };
        self.stack.push(ExprNode::SimpleCase {
            operand,
            arms: case_arms,
            else_,
            result: result.cloned(),
        });
        Ok(())
    }

    fn visit_unary_fn<O>(&mut self, func: UnaryStringFunc, operand: O) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
    {
        let func = match func {
            UnaryStringFunc::Lower => ScalarFunc::Lower,
            UnaryStringFunc::Upper => ScalarFunc::Upper,
            UnaryStringFunc::Length => ScalarFunc::Length,
            UnaryStringFunc::Trim => ScalarFunc::Trim,
        };
        let arg = self.child(operand)?;
        self.stack.push(ExprNode::ScalarFn {
            func,
            args: vec![*arg],
        });
        Ok(())
    }

    fn visit_concat<L, R>(&mut self, left: L, right: R) -> io::Result<()>
    where
        L: FnOnce(&mut Self) -> io::Result<()>,
        R: FnOnce(&mut Self) -> io::Result<()>,
    {
        let left = self.child(left)?;
        let right = self.child(right)?;
        self.stack.push(ExprNode::ScalarFn {
            func: ScalarFunc::Concat,
            args: vec![*left, *right],
        });
        Ok(())
    }

    fn visit_substring<S, Start, Len>(
        &mut self,
        string: S,
        start: Start,
        len: Len,
    ) -> io::Result<()>
    where
        S: FnOnce(&mut Self) -> io::Result<()>,
        Start: FnOnce(&mut Self) -> io::Result<()>,
        Len: FnOnce(&mut Self) -> io::Result<()>,
    {
        let string = self.child(string)?;
        let start = self.child(start)?;
        let len = self.child(len)?;
        self.stack.push(ExprNode::ScalarFn {
            func: ScalarFunc::Substring,
            args: vec![*string, *start, *len],
        });
        Ok(())
    }

    fn visit_now(&mut self) -> io::Result<()> {
        self.stack.push(ExprNode::Now);
        Ok(())
    }

    fn visit_extract<O>(
        &mut self,
        field: DateField,
        operand: O,
        cast: &SqlType,
        timezone: Option<&str>,
        _operand_cast: Option<&SqlType>,
    ) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
    {
        // A view body inlines literals (no placeholders), so the operand type anchor is unneeded here.
        let operand = self.child(operand)?;
        self.stack.push(ExprNode::Extract {
            field,
            operand,
            result: Some(cast.clone()),
            timezone: timezone.map(str::to_owned),
        });
        Ok(())
    }

    fn visit_date_trunc<O>(
        &mut self,
        unit: DateField,
        operand: O,
        timezone: Option<&str>,
        _operand_cast: Option<&SqlType>,
    ) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
    {
        let operand = self.child(operand)?;
        self.stack.push(ExprNode::DateTrunc {
            unit,
            operand,
            timezone: timezone.map(str::to_owned),
        });
        Ok(())
    }

    fn visit_extract_second<O>(
        &mut self,
        operand: O,
        cast: &SqlType,
        _operand_cast: Option<&SqlType>,
    ) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
    {
        let operand = self.child(operand)?;
        self.stack.push(ExprNode::ExtractSecond {
            operand,
            result: Some(cast.clone()),
        });
        Ok(())
    }

    fn visit_case_when(&mut self) -> io::Result<()> {
        // Arm boundaries are recovered structurally from the node stack (see `visit_case`).
        Ok(())
    }

    fn visit_case_then(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn visit_case_value_open(&mut self, _cast: Option<&SqlType>) -> io::Result<()> {
        // The cast is captured in `ExprNode::Case::result` and applied per branch by the view renderer.
        Ok(())
    }

    fn visit_case_value_close(&mut self, _cast: Option<&SqlType>) -> io::Result<()> {
        Ok(())
    }
}

impl PredicateAstVisitor for IrBuilder {
    fn visit_compare<L, R>(&mut self, op: CompareOp, left: L, right: R) -> io::Result<()>
    where
        L: FnOnce(&mut Self) -> io::Result<()>,
        R: FnOnce(&mut Self) -> io::Result<()>,
    {
        let left = self.child(left)?;
        let right = self.child(right)?;
        self.stack.push(ExprNode::Compare { op, left, right });
        Ok(())
    }

    fn visit_and<L, R>(&mut self, left: L, right: R) -> io::Result<()>
    where
        L: FnOnce(&mut Self) -> io::Result<()>,
        R: FnOnce(&mut Self) -> io::Result<()>,
    {
        let left = self.child(left)?;
        let right = self.child(right)?;
        self.stack.push(ExprNode::Logical {
            op: LogicalOp::And,
            left,
            right,
        });
        Ok(())
    }

    fn visit_or<L, R>(&mut self, left: L, right: R) -> io::Result<()>
    where
        L: FnOnce(&mut Self) -> io::Result<()>,
        R: FnOnce(&mut Self) -> io::Result<()>,
    {
        let left = self.child(left)?;
        let right = self.child(right)?;
        self.stack.push(ExprNode::Logical {
            op: LogicalOp::Or,
            left,
            right,
        });
        Ok(())
    }

    fn visit_not<P>(&mut self, predicate: P) -> io::Result<()>
    where
        P: FnOnce(&mut Self) -> io::Result<()>,
    {
        let operand = self.child(predicate)?;
        self.stack.push(ExprNode::Not(operand));
        Ok(())
    }

    fn visit_is_null<O>(&mut self, negated: bool, operand: O) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
    {
        let operand = self.child(operand)?;
        self.stack.push(ExprNode::IsNull { negated, operand });
        Ok(())
    }

    fn visit_like<O, P>(
        &mut self,
        case_insensitive: bool,
        negated: bool,
        operand: O,
        pattern: P,
    ) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
        P: FnOnce(&mut Self) -> io::Result<()>,
    {
        let operand = self.child(operand)?;
        let pattern = self.child(pattern)?;
        self.stack.push(ExprNode::Like {
            case_insensitive,
            negated,
            operand,
            pattern,
        });
        Ok(())
    }

    fn visit_in<O, T>(&mut self, negated: bool, operand: O, values: &[T]) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
        T: Encode<ModelBackend>,
    {
        let operand = self.child(operand)?;
        let items = values
            .iter()
            .map(|value| ExprNode::Literal(encode_literal(value)))
            .collect();
        self.stack.push(ExprNode::In {
            negated,
            operand,
            items,
        });
        Ok(())
    }

    fn visit_between<O, Lo, Hi>(
        &mut self,
        negated: bool,
        operand: O,
        lo: Lo,
        hi: Hi,
    ) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
        Lo: FnOnce(&mut Self) -> io::Result<()>,
        Hi: FnOnce(&mut Self) -> io::Result<()>,
    {
        let operand = self.child(operand)?;
        let low = self.child(lo)?;
        let high = self.child(hi)?;
        self.stack.push(ExprNode::Between {
            negated,
            operand,
            low,
            high,
        });
        Ok(())
    }

    fn visit_bool_test<O>(&mut self, negated: bool, operand: O) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
    {
        let operand = self.child(operand)?;
        self.stack.push(if negated {
            ExprNode::Not(operand)
        } else {
            *operand
        });
        Ok(())
    }

    fn visit_in_subquery<O, Sub>(
        &mut self,
        negated: bool,
        operand: O,
        subquery: &Sub,
    ) -> io::Result<()>
    where
        O: FnOnce(&mut Self) -> io::Result<()>,
        Sub: RenderSubquery<ModelBackend>,
    {
        let operand = self.child(operand)?;
        self.stack.push(ExprNode::InSubquery {
            negated,
            operand,
            subquery: Box::new(lower_subquery(subquery)?),
        });
        Ok(())
    }

    fn visit_exists<Sub>(&mut self, negated: bool, subquery: &Sub) -> io::Result<()>
    where
        Sub: RenderSubquery<ModelBackend>,
    {
        self.stack.push(ExprNode::Exists {
            negated,
            subquery: Box::new(lower_subquery(subquery)?),
        });
        Ok(())
    }
}

fn build_expr<K, Ast>(expr: &Expr<'_, K, Ast>) -> io::Result<ExprNode>
where
    K: ExprKind,
    Ast: RenderAst<ModelBackend>,
{
    let mut builder = IrBuilder::default();
    expr.visit(&mut builder)?;
    Ok(builder.finish())
}

fn build_column<K>(column: ColumnRef<'_, K>) -> io::Result<ExprNode>
where
    K: ExprKind,
{
    let mut builder = IrBuilder::default();
    column.visit(&mut builder)?;
    Ok(builder.finish())
}

fn build_predicate<P, Ast>(predicate: &Predicate<'_, P, Ast>) -> io::Result<ExprNode>
where
    P: PredicateKind,
    Ast: RenderPredicateAst<ModelBackend>,
{
    let mut builder = IrBuilder::default();
    predicate.visit(&mut builder)?;
    Ok(builder.finish())
}

fn build_order<K, Ast>(order: &Order<'_, K, Ast>) -> io::Result<ExprNode>
where
    K: ExprKind,
    Ast: RenderAst<ModelBackend>,
{
    let mut builder = IrBuilder::default();
    order.visit_expr(&mut builder)?;
    Ok(builder.finish())
}

/// Lowers a nested subquery into its own [`ViewQueryModel`], reusing the structural lowering.
fn lower_subquery<Sub>(subquery: &Sub) -> io::Result<ViewQueryModel>
where
    Sub: RenderSubquery<ModelBackend>,
{
    let mut sink = ModelSink::default();
    subquery.lower_subquery(&mut sink)?;
    Ok(sink.query)
}

/// Merges `node` into `slot` with `AND` (repeated `WHERE`/`HAVING` predicates).
fn and_into(slot: &mut Option<ExprNode>, node: ExprNode) {
    *slot = Some(match slot.take() {
        Some(previous) => ExprNode::Logical {
            op: LogicalOp::And,
            left: Box::new(previous),
            right: Box::new(node),
        },
        None => node,
    });
}

fn source_ref<S>(alias: SourceAlias) -> SourceRef
where
    S: TableProjection,
{
    SourceRef {
        schema: <S as TableProjection>::schema_name().map(str::to_owned),
        name: <S as TableProjection>::name().to_owned(),
        alias: alias.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The sink that captures a view's structural body
// ---------------------------------------------------------------------------

/// A [`SelectSink`] that records a query's structure into a [`ViewQueryModel`] instead of emitting a
/// flat `SELECT` string. Scalar expressions are captured structurally as [`ExprNode`] trees.
#[derive(Default)]
pub(crate) struct ModelSink {
    query: ViewQueryModel,
}

impl SelectSink for ModelSink {
    type Error = io::Error;
    type Backend = ModelBackend;

    fn push_projection<Shape, P>(&mut self, projection: P) -> Result<(), Self::Error>
    where
        Shape: ProjectionShape,
        P: RenderProjectable<ModelBackend>,
    {
        projection.visit_projection(self)
    }

    fn push_table_source<S>(&mut self, alias: SourceAlias) -> Result<(), Self::Error>
    where
        S: TableProjection,
    {
        self.query.from = Some(source_ref::<S>(alias));
        Ok(())
    }

    fn push_inner_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let on = build_predicate(&on)?;
        self.query.joins.push(JoinItem {
            kind: JoinKind::Inner,
            source: source_ref::<S>(alias),
            on: Some(on),
        });
        Ok(())
    }

    fn push_left_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let on = build_predicate(&on)?;
        self.query.joins.push(JoinItem {
            kind: JoinKind::Left,
            source: source_ref::<S>(alias),
            on: Some(on),
        });
        Ok(())
    }

    fn push_right_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let on = build_predicate(&on)?;
        self.query.joins.push(JoinItem {
            kind: JoinKind::Right,
            source: source_ref::<S>(alias),
            on: Some(on),
        });
        Ok(())
    }

    fn push_full_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let on = build_predicate(&on)?;
        self.query.joins.push(JoinItem {
            kind: JoinKind::Full,
            source: source_ref::<S>(alias),
            on: Some(on),
        });
        Ok(())
    }

    fn push_cross_join<S>(&mut self, alias: SourceAlias) -> Result<(), Self::Error>
    where
        S: TableProjection,
    {
        self.query.joins.push(JoinItem {
            kind: JoinKind::Cross,
            source: source_ref::<S>(alias),
            on: None,
        });
        Ok(())
    }

    fn push_filter<P, Ast>(&mut self, predicate: Predicate<'_, P, Ast>) -> Result<(), Self::Error>
    where
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let node = build_predicate(&predicate)?;
        and_into(&mut self.query.filter, node);
        Ok(())
    }

    fn push_group<K, Ast>(&mut self, key: &Expr<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<ModelBackend>,
    {
        let key = build_expr(key)?;
        self.query.group_by.push(key);
        Ok(())
    }

    fn push_having<P, Ast>(&mut self, predicate: Predicate<'_, P, Ast>) -> Result<(), Self::Error>
    where
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let node = build_predicate(&predicate)?;
        and_into(&mut self.query.having, node);
        Ok(())
    }

    fn push_order<K, Ast>(&mut self, order: Order<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<ModelBackend>,
    {
        let expr = build_order(&order)?;
        self.query.order_by.push(OrderItem {
            expr,
            direction: Some(order.direction()),
            nulls: None,
        });
        Ok(())
    }

    fn set_limit(&mut self, rows: usize) -> Result<(), Self::Error> {
        self.query.limit = Some(rows);
        Ok(())
    }

    fn set_offset(&mut self, rows: usize) -> Result<(), Self::Error> {
        self.query.offset = Some(rows);
        Ok(())
    }

    fn set_distinct(&mut self) -> Result<(), Self::Error> {
        self.query.distinct = true;
        Ok(())
    }
}

impl ProjectionVisitor for ModelSink {
    type Error = io::Error;
    type Backend = ModelBackend;

    fn visit_expr<K, Ast>(
        &mut self,
        expr: &Expr<'_, K, Ast>,
        alias: Cow<'static, str>,
    ) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<ModelBackend>,
    {
        let expr = build_expr(expr)?;
        self.query.projection.push(ProjectionItem {
            output_name: alias.into_owned(),
            expr,
        });
        Ok(())
    }

    fn visit_column<K>(
        &mut self,
        column: ColumnRef<'_, K>,
        alias: Cow<'static, str>,
    ) -> Result<(), Self::Error>
    where
        K: ExprKind,
    {
        let expr = build_column(column)?;
        self.query.projection.push(ProjectionItem {
            output_name: alias.into_owned(),
            expr,
        });
        Ok(())
    }
}

/// Lowers a typed, projected query (built against [`ModelConn`]) into a neutral [`ViewQueryModel`].
#[doc(hidden)]
pub fn lower_view<'conn, 'scope, Base, Shape, Projection>(
    selected: &Selected<'scope, Base, Shape, Projection>,
) -> ViewQueryModel
where
    Base: RenderSelectAst<'conn, 'scope, ModelConn, ModelBackend>,
    Shape: ProjectionShape,
    Projection: RenderProjectable<ModelBackend>,
{
    let mut sink = ModelSink::default();
    selected
        .lower_into::<ModelConn, _>(&mut sink)
        .expect("rendering a view body to memory cannot fail");
    sink.query
}

// ---------------------------------------------------------------------------
// View definition surface
// ---------------------------------------------------------------------------

/// A projected query usable as a view body. Implemented by [`Selected`] when it is built against
/// [`ModelConn`]; [`Self::Row`] is the projection's decoded row type, which the compile-time check in
/// [`ViewDefinition`] pins to the view's declared columns.
pub trait ViewSelect {
    /// The decoded row type of the projection (the ordered column types).
    type Row;

    /// Lower the query body into the neutral model.
    fn lower(&self) -> ViewQueryModel;
}

impl<'scope, Base, Shape, Projection> ViewSelect for Selected<'scope, Base, Shape, Projection>
where
    Shape: ProjectionShape,
    Projection: Projectable + RenderProjectable<ModelBackend>,
    Base: RenderSelectAst<'static, 'scope, ModelConn, ModelBackend>,
    // A view body has no bind parameters — every value is inlined as a literal. Requiring an empty
    // runtime-parameter shape rejects a definition that uses `param::<K>()` at compile time, rather
    // than silently dropping the placeholder and emitting invalid DDL.
    <Base as SelectAst<'static, 'scope, ModelConn>>::Params: crate::NoRuntimeParams,
{
    type Row = Shape::Row;

    fn lower(&self) -> ViewQueryModel {
        lower_view(self)
    }
}

/// A view's declared output schema: its name, namespace, and typed columns. `#[derive(View)]`
/// generates this from the struct fields; it is the view analogue of `SchemaTable`.
pub trait SchemaView {
    /// The declared output row type (the ordered column types), matched against the body's projection.
    type Row;

    fn schema_name() -> Option<&'static str>;

    fn view_name() -> &'static str;

    fn view_columns() -> Vec<crate::ViewColumnModel>;
}

/// The user-written body of a view: the `SELECT` that produces its declared columns. The metadata
/// comes from [`SchemaView`] (generated by `#[derive(View)]`), so the user writes only
/// [`definition`](Self::definition), against [`ModelConn`] using
/// [`project`](crate::SourceQuery::project).
///
/// The return type `impl ViewSelect<Row = <Self as SchemaView>::Row>` is the compile-time guarantee:
/// the body's projection must decode to the same row type as the declared columns, so a mismatch is a
/// type error.
pub trait ViewDefinition: SchemaView {
    /// The view body, built against the render-only connection. The `&'static` borrow keeps the
    /// resulting query fully owned (no caller-tied lifetime), which is all the model walker needs.
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row>;
}

/// Any [`ViewDefinition`] is an object-safe [`ViewDef`](crate::ViewDef) the model walker can consume.
impl<T> crate::ViewDef for T
where
    T: ViewDefinition + Sync,
{
    fn schema_name(&self) -> Option<&'static str> {
        <T as SchemaView>::schema_name()
    }

    fn name(&self) -> &'static str {
        <T as SchemaView>::view_name()
    }

    fn columns(&self) -> Vec<crate::ViewColumnModel> {
        <T as SchemaView>::view_columns()
    }

    fn definition_model(&self) -> ViewQueryModel {
        view_definition_model::<T>()
    }
}

/// Lowers a [`ViewDefinition`] type's body into the neutral model without needing an instance. The
/// `#[derive(Schema)]`-generated `ViewDef` shims call this so a view registers from its type alone.
#[doc(hidden)]
pub fn view_definition_model<T>() -> ViewQueryModel
where
    T: ViewDefinition,
{
    static MODEL_CONN: ModelConn = ModelConn;
    T::definition(&MODEL_CONN).lower()
}

// ---------------------------------------------------------------------------
// Stub `QueryBuilder`: only the type machinery, never constructed
// ---------------------------------------------------------------------------

macro_rules! never_query {
    ($name:ident) => {
        #[doc(hidden)]
        pub struct $name<T: ?Sized>(PhantomData<T>, ModelNever);
    };
}

never_query!(ModelSelect);
never_query!(ModelInsert);
never_query!(ModelUpdate);
never_query!(ModelDelete);

impl<'builder, 'scope, Base, Shape, Projection>
    crate::SelectQuery<'builder, 'scope, Base, Projection>
    for ModelSelect<(&'builder (), &'scope (), Base, Shape, Projection)>
where
    Base: SelectAst<'builder, 'scope, ModelConn>,
    Shape: ProjectionShape,
    Shape::Row: Decode<ModelBackend> + Send,
    Projection: Projectable,
{
    type Builder = ModelConn;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build_selected(
        _builder: &'builder ModelConn,
        _selected: Selected<'scope, Base, Shape, Projection>,
    ) -> Self {
        unreachable!("ModelConn never builds a select")
    }
}

impl<'builder, S, Shape, Rows, Returning> crate::InsertQuery<'builder, Rows, Returning>
    for ModelInsert<(&'builder (), S, Shape, Rows, Returning)>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<ModelBackend> + Send,
    Rows: crate::InsertRows,
    Returning: Projectable,
{
    type Builder = ModelConn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(_builder: &'builder ModelConn, _rows: Rows, _returning: Returning) -> Self {
        unreachable!("ModelConn never builds an insert")
    }
}

impl<'builder, S, Shape, Columns, Filters, Returning>
    crate::UpdateQuery<'builder, Columns, Filters, Returning>
    for ModelUpdate<(&'builder (), S, Shape, Columns, Filters, Returning)>
where
    S: crate::UpdateableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<ModelBackend> + Send,
    Columns: crate::UpdateAssignments,
    Filters: crate::PredicateNodes,
    Returning: Projectable,
{
    type Builder = ModelConn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(
        _builder: &'builder ModelConn,
        _alias: SourceAlias,
        _columns: Columns,
        _filters: Filters,
        _returning: Returning,
    ) -> Self {
        unreachable!("ModelConn never builds an update")
    }
}

impl<'builder, S, Shape, Filters, Returning> crate::DeleteQuery<'builder, Filters, Returning>
    for ModelDelete<(&'builder (), S, Shape, Filters, Returning)>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Shape::Row: Decode<ModelBackend> + Send,
    Filters: crate::PredicateNodes,
    Returning: Projectable,
{
    type Builder = ModelConn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(
        _builder: &'builder ModelConn,
        _alias: SourceAlias,
        _filters: Filters,
        _returning: Returning,
    ) -> Self {
        unreachable!("ModelConn never builds a delete")
    }
}

impl QueryBuilder for ModelConn {
    type Backend = ModelBackend;

    type Select<'builder, 'scope, Base, Shape, Projection>
        = ModelSelect<(&'builder (), &'scope (), Base, Shape, Projection)>
    where
        Self: 'builder,
        Base: 'builder,
        Base: SelectAst<'builder, 'scope, Self>,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Projection: Projectable;

    type Insert<'builder, S, Shape, Rows, Returning>
        = ModelInsert<(&'builder (), S, Shape, Rows, Returning)>
    where
        Self: 'builder,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Rows: crate::InsertRows,
        Returning: Projectable;

    type Update<'builder, S, Shape, Columns, Filters, Returning>
        = ModelUpdate<(&'builder (), S, Shape, Columns, Filters, Returning)>
    where
        Self: 'builder,
        S: crate::UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Columns: crate::UpdateAssignments,
        Filters: crate::PredicateNodes,
        Returning: Projectable;

    type Delete<'builder, S, Shape, Filters, Returning>
        = ModelDelete<(&'builder (), S, Shape, Filters, Returning)>
    where
        Self: 'builder,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Filters: crate::PredicateNodes,
        Returning: Projectable;
}
