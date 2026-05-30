/// A fixed or dynamic homogeneous list used by core IR builders.
pub trait IrList<T> {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn try_for_each<E>(&self, f: impl FnMut(&T) -> Result<(), E>) -> Result<(), E>;

    fn into_vec(self) -> Vec<T>
    where
        Self: Sized;
}

/// Append an item to a typed list, producing the widened list type.
pub trait TupleAppend<T>: IrList<T> + Sized {
    type Output: IrList<T>;

    fn append(self, value: T) -> Self::Output;
}

/// Concatenate two typed lists.
pub trait TupleConcat<T, Rhs>: IrList<T> + Sized
where
    Rhs: IrList<T>,
{
    type Output: IrList<T>;

    fn concat(self, rhs: Rhs) -> Self::Output;
}

/// Append a value to a heterogeneous tuple.
pub trait TuplePush<T>: Sized {
    type Output;

    fn push(self, value: T) -> Self::Output;
}

/// Marker for heterogeneous tuples whose width is known in the type system.
pub trait TupleLen<const N: usize> {
    const LEN: usize = N;
}

impl<T> IrList<T> for () {
    fn len(&self) -> usize {
        0
    }

    fn try_for_each<E>(&self, _f: impl FnMut(&T) -> Result<(), E>) -> Result<(), E> {
        Ok(())
    }

    fn into_vec(self) -> Vec<T> {
        Vec::new()
    }
}

impl<T> TupleAppend<T> for () {
    type Output = (T,);

    fn append(self, value: T) -> Self::Output {
        (value,)
    }
}

impl<T, Rhs> TupleConcat<T, Rhs> for ()
where
    Rhs: IrList<T>,
{
    type Output = Rhs;

    fn concat(self, rhs: Rhs) -> Self::Output {
        rhs
    }
}

impl<T> TuplePush<T> for () {
    type Output = (T,);

    fn push(self, value: T) -> Self::Output {
        (value,)
    }
}

impl TupleLen<0> for () {}

impl<T> IrList<T> for Vec<T> {
    fn len(&self) -> usize {
        self.len()
    }

    fn try_for_each<E>(&self, mut f: impl FnMut(&T) -> Result<(), E>) -> Result<(), E> {
        for item in self {
            f(item)?;
        }
        Ok(())
    }

    fn into_vec(self) -> Vec<T> {
        self
    }
}

impl<T> TupleAppend<T> for Vec<T> {
    type Output = Vec<T>;

    fn append(mut self, value: T) -> Self::Output {
        self.push(value);
        self
    }
}

impl<T, Rhs> TupleConcat<T, Rhs> for Vec<T>
where
    Rhs: IrList<T>,
{
    type Output = Vec<T>;

    fn concat(mut self, rhs: Rhs) -> Self::Output {
        self.extend(rhs.into_vec());
        self
    }
}

squealy_macros::tuple_ir_lists!(32);
squealy_macros::tuple_lens!(32);
squealy_macros::tuple_pushes!(32);
