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

/// Empty heterogeneous list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HNil;

/// Non-empty heterogeneous list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HCons<Head, Tail> {
    pub head: Head,
    pub tail: Tail,
}

/// A heterogeneous list whose width is known in the type system.
pub trait HList {
    const LEN: usize;
}

/// Append a value to the end of a heterogeneous list.
pub trait PushBack<T>: HList + Sized {
    type Output: HList;

    fn push_back(self, value: T) -> Self::Output;
}

/// Convert a heterogeneous list to a same-order tuple.
pub trait ToTuple {
    type Tuple;

    fn to_tuple(self) -> Self::Tuple;
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

impl HList for HNil {
    const LEN: usize = 0;
}

impl<Head, Tail> HList for HCons<Head, Tail>
where
    Tail: HList,
{
    const LEN: usize = Tail::LEN + 1;
}

impl<T> PushBack<T> for HNil {
    type Output = HCons<T, HNil>;

    fn push_back(self, value: T) -> Self::Output {
        HCons {
            head: value,
            tail: HNil,
        }
    }
}

impl<Head, Tail, T> PushBack<T> for HCons<Head, Tail>
where
    Tail: PushBack<T>,
{
    type Output = HCons<Head, <Tail as PushBack<T>>::Output>;

    fn push_back(self, value: T) -> Self::Output {
        HCons {
            head: self.head,
            tail: self.tail.push_back(value),
        }
    }
}

impl ToTuple for HNil {
    type Tuple = ();

    fn to_tuple(self) -> Self::Tuple {}
}

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
squealy_macros::hlist_tuples!(32);
