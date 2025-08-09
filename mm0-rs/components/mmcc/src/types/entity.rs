//! The `Entity` type, which describes processed and typechecked
//! previous declarations, in addition to intrinsics and primops.

use std::collections::HashMap;
#[cfg(feature = "memory")] use mm0_deepsize_derive::DeepSizeOf;
use crate::{Compiler, FileSpan, Symbol, intern, symbol::{Interner, init_dense_symbol_map},
  types::{ast, global, Idx}};
use super::Spanned;

macro_rules! make_prims {
  {$($(#[$attr0:meta])* enum $name:ident {
    $($(#[$attr:meta])* $x:ident $($mark:literal)?: $e:expr,)*
  })* } => {
    $(
      $(#[$attr0])*
      #[derive(Debug, PartialEq, Eq, Copy, Clone)]
      pub enum $name { $($(#[$attr])* $x),* }
      #[cfg(feature = "memory")] mm0_deepsize::deep_size_0!($name);

      impl $name {
        /// Evaluate a function on all elements of the type, with their names.
        pub fn scan(#[allow(unused)] mut f: impl FnMut(Self, &'static str)) {
          $(f($name::$x, $e);)*
        }
        /// Convert a string into this type.
        #[allow(clippy::should_implement_trait)]
        #[must_use] pub fn from_str(s: &str) -> Option<Self> {
          match s {
            $($e => Some(Self::$x),)*
            _ => None
          }
        }

        /// Get the MMC keyword for a symbol.
        #[must_use] pub fn from_symbol(s: Symbol) -> Option<Self> {
          use std::sync::LazyLock;
          static SYMBOL_MAP: LazyLock<Box<[Option<$name>]>> = LazyLock::new(|| {
            init_dense_symbol_map(&[$((intern($e), $name::$x)),*])
          });
          SYMBOL_MAP.get(s.into_usize()).map_or(None, |x| *x)
        }

        /// Get the symbol for this primitive.
        #[must_use] pub fn as_symbol(self) -> Symbol {
          use std::sync::LazyLock;
          static INTERNED: LazyLock<[Symbol; <[()]>::len(&[$(() $($mark)?),*])]> =
            LazyLock::new(|| [$(intern($e)),*]);
          INTERNED[self as usize]
        }

        /// Convert a byte string into this type.
        #[must_use] pub fn from_bytes(s: &[u8]) -> Option<Self> {
          // Safety: the function we defined just above doesn't do anything
          // dangerous with the &str
          Self::from_str(unsafe { std::str::from_utf8_unchecked(s) })
        }
      }
    )*
  }
}

make_prims! {
  /// The primitive operations.
  enum PrimOp {
    /// `{x + y}` returns the integer sum of the arguments
    Add: "+",
    /// `(and x1 ... xn)` returns the boolean `AND` of the arguments.
    And: "and",
    /// `{x as T}` performs truncation and non-value preserving casts a la `reinterpret_cast`.
    As: "as",
    /// `(assert p)` evaluates `p` and if it is false, crashes the program with an error.
    /// It returns a proof that `p` is true (because if `p` is false then the
    /// rest of the function is not evaluated).
    Assert: "assert",
    /// `(band e1 ... en)` returns the bitwise `AND` of the arguments.
    BitAnd: "band",
    /// `(bnot e1 ... en)` returns the bitwise `NOR` of the arguments,
    /// usually used in the unary case as `NOT`
    BitNot: "bnot",
    /// `(bor e1 ... en)` returns the bitwise `OR` of the arguments.
    BitOr: "bor",
    /// * `(break e)` jumps out of the nearest enclosing loop, returning `e` to the enclosing scope.
    /// * `(break lab e)` jumps out of the scope containing label `lab`,
    ///   returning `e` as the result of the block.
    Break: "break",
    /// `(bxor e1 ... en)` returns the bitwise `XOR` of the arguments.
    BitXor: "bxor",
    /// `(& x)` constructs a reference to `x`.
    Borrow: "&",
    /// `{(cast {x : T} h) : U}` returns `x` of type `U` if `h` proves `x :> T -* x :> U`.
    Cast: "cast",
    /// * `(continue e)` jumps to the start of the nearest enclosing loop.
    /// * `(continue lab e)` jumps to the start of the loop with label `lab`.
    Continue: "continue",
    /// `{x = y}` returns true if `x` is equal to `y`
    Eq: "=",
    /// `(ghost x)` returns the same thing as `x` but in the type `(ghost A)`.
    Ghost: "ghost",
    /// The function `(index a i h)` is the equivalent of `C`'s `a[i]`;
    /// it has type `(own T)` if `a` has type `(own (array T i))` and type `(& T)`
    /// if `a` has type `(& (array T i))`. The hypothesis `h` is a proof that
    /// `i` is in the bounds of the array.
    Index: "index",
    /// `{x max y}` returns the maximum of the arguments
    Max: "max",
    /// `{x min y}` returns the minimum of the arguments
    Min: "min",
    /// * `{x * y}` returns the integer product of the arguments
    /// * `(* x)` is a deref operation `*x: T` where `x: &T`.
    MulDeref: "*",
    /// `{x != y}` returns true if `x` is not equal to `y`
    Ne: "!=",
    /// `(not x1 ... xn)` returns the boolean `NOR` of the arguments,
    /// usually used in the unary case as `NOT`
    Not: "not",
    /// `(or x1 ... xn)` returns the boolean `OR` of the arguments.
    Or: "or",
    /// `{x <= y}` returns true if `x` is less than or equal to `y`
    Le: "<=",
    /// `(list e1 ... en)` returns a tuple of the arguments.
    List: "list",
    /// `{x < y}` returns true if `x` is less than `y`
    Lt: "<",
    /// `(pure $e$)` embeds an MM0 expression `$e$` as the target type,
    /// one of the numeric types
    Pure: "pure",
    /// `(pun x h)` returns a value of type `T` if `h` proves `x` has type `T`.
    Pun: "pun",
    /// `(ref x)` constructs `x` as an lvalue (place).
    Ref: "ref",
    /// `(return e1 ... en)` returns `e1, ..., en` from the current function.
    Return: "return",
    /// `(sizeof T)` is the size of `T` in bytes.
    Sizeof: "sizeof",
    /// If `x: (array T n)`, then `(slice x b h): (array T a)` if
    /// `h` is a proof that `b + a <= n`. Computationally this corresponds to
    /// simply `&x + b * sizeof T`, in the manner of C pointer arithmetic.
    Slice: "slice",
    /// `{a shl b}` computes the value `a * 2 ^ b`, a left shift of `a` by `b`.
    Shl: "shl",
    /// `{a shr b}` computes the value `a // 2 ^ b`, a right shift of `a` by `b`.
    /// This is an arithmetic shift on signed integers and a logical shift on unsigned integers.
    Shr: "shr",
    /// `(sn x)` constructs the unique element of the singleton type `(sn x)`.
    Sn: "sn",
    /// * `(- x)` returns the negative of the argument
    /// * `{x - y}` returns the integer difference of the arguments
    Sub: "-",
    /// `{e : T}` is `e`, with the type `T`. This is used only to direct
    /// type inference, it has no effect otherwise.
    Typed: ":",
    /// If `x` has type `T`, then `(typeof! x)` is a proof that `x :> T`.
    /// This consumes `x`'s type.
    TypeofBang: "typeof!",
    /// If `x` has type `T` where `T` is a copy type, then `(typeof x)` is a
    /// proof that `x :> T`. This version of `typeof!` only works on copy types
    /// so that it doesn't consume `x`.
    Typeof: "typeof",
    /// `{uninit : (? T)}` is an effectful expression producing an undefined value
    /// in any `(? T)` type.
    Uninit: "uninit",
    /// `(unreachable h)` takes a proof of false and undoes the current code path.
    Unreachable: "unreachable",
  }

  /// The primitive types.
  enum PrimType {
    /// `A. {x : A} p` or `(al {x : A} p)` is universal quantification over a type.
    All: "al",
    /// `(and A B C)` is an intersection type of `A, B, C`;
    /// `sizeof (and A B C) = max (sizeof A, sizeof B, sizeof C)`, and
    /// the typehood predicate is `x :> (inter A B C)` iff
    /// `x :> A /\ x :> B /\ x :> C`. (Note that this is regular conjunction,
    /// not separating conjunction.)
    And: "and",
    /// The type `(array T n)` is an array of `n` elements of type `T`;
    /// `sizeof (array T n) = sizeof T * n`.
    Array: "array",
    /// `bool` is the type of booleans, that is, bytes which are 0 or 1; `sizeof bool = 1`.
    Bool: "bool",
    /// `E. {x : A} p` or `(ex {x : A} p)` is existential quantification over a type.
    Ex: "ex",
    /// `(ghost A)` is a compoutationally irrelevant version of `A`, which means
    /// that the logical storage of `(ghost A)` is the same as `A` but the physical storage
    /// is the same as `()`. `sizeof (ghost A) = 0`.
    Ghost: "ghost",
    /// `x :> T` is the proposition that asserts that `x` has type `T`.
    HasTy: ":>",
    /// `i8` is the type of 8 bit signed integers; `sizeof i8 = 1`.
    I8: "i8",
    /// `i16` is the type of 16 bit signed integers; `sizeof i16 = 2`.
    I16: "i16",
    /// `i32` is the type of 32 bit signed integers; `sizeof i32 = 4`.
    I32: "i32",
    /// `i64` is the type of 64 bit signed integers; `sizeof i64 = 8`.
    I64: "i64",
    /// `p -> q` is (regular) implication.
    Imp: "->",
    /// The input token (passed to functions that read from input)
    Input: "Input",
    /// `int` is the type of unbounded signed integers; `sizeof int = inf`.
    Int: "int",
    /// `(A, B, C)` is a tuple type with elements `A, B, C`;
    /// `sizeof (A, B, C) = sizeof A + sizeof B + sizeof C`.
    List: "list",
    /// `(moved T)` is the moved version of `T`, the duplicable core of the type.
    Moved: "moved",
    /// `nat` is the type of unbounded unsigned integers; `sizeof nat = inf`.
    Nat: "nat",
    /// `(or A B C)` is an undiscriminated anonymous union of types `A, B, C`.
    /// `sizeof (or A B C) = max (sizeof A, sizeof B, sizeof C)`, and
    /// the typehood predicate is `x :> (or A B C)` iff
    /// `x :> A \/ x :> B \/ x :> C`.
    Or: "or",
    /// The output token (passed to functions that produce output)
    Output: "Output",
    /// `own T` is a type of owned pointers. The typehood predicate is
    /// `x :> own T` iff `E. v: T, x |-> v`.
    Own: "own",
    /// `(ref T)` is a type of borrowed values. This type is elaborated to
    /// `(ref a T)` where `a` is a lifetime; this is handled a bit differently than rust
    /// (see [`Lifetime`](super::ty::Lifetime)).
    Ref: "ref",
    /// `&sn e` is a type of pointers to a place `e`.
    /// This type has the property that if `x: &sn e` then `*x` evaluates to
    /// the place `e`, which can be read or written.
    RefSn: "&sn",
    /// `(& T)` is a type of borrowed pointers. This type is elaborated to
    /// `(& a T)` where `a` is a lifetime; this is handled a bit differently than rust
    /// (see [`Lifetime`](super::ty::Lifetime)).
    Shr: "&",
    /// `(sn {a : T})` the type of values of type `T` that are equal to `a`.
    /// This is useful for asserting that a computationally relevant value can be
    /// expressed in terms of computationally irrelevant parts.
    Sn: "sn",
    /// `p * q` is separating conjunction.
    Star: "*",
    /// `{x : A, y : B, z : C}` is the dependent version of `list`;
    /// it is a tuple type with elements `A, B, C`, but the types `A, B, C` can
    /// themselves refer to `x, y, z`.
    /// `sizeof {x : A, _ : B x} = sizeof A + max_x (sizeof (B x))`.
    ///
    /// The top level declaration `(struct foo {x : A} {y : B})` desugars to
    /// `(typedef foo {x : A, y : B})`.
    Struct: "struct",
    /// `u8` is the type of 8 bit unsigned integers; `sizeof u8 = 1`.
    U8: "u8",
    /// `u16` is the type of 16 bit unsigned integers; `sizeof u16 = 2`.
    U16: "u16",
    /// `u32` is the type of 32 bit unsigned integers; `sizeof u32 = 4`.
    U32: "u32",
    /// `u64` is the type of 64 bit unsigned integers; `sizeof u64 = 8`.
    U64: "u64",
    /// `(? T)` is the type of possibly-uninitialized `T`s. The typing predicate
    /// for this type is vacuous, but it has the same size as `T`, so overwriting with
    /// a `T` is possible.
    Uninit: "?",
    /// `p -* q` is separating implication.
    Wand: "-*",
  }

  /// Intrinsic functions, which are like [`PrimOp`] but are typechecked like regular
  /// function calls.
  enum IntrinsicProc {
    /// Intrinsic for the [`open`](https://man7.org/linux/man-pages/man2/open.2.html) system call,
    /// for the reading case.
    /// ```text
    /// intrinsic proc sys_open(filename: &CStr) -> u32;
    /// ```
    Open: "sys_open",
    /// Intrinsic for the [`open`](https://man7.org/linux/man-pages/man2/open.2.html) system call,
    /// for the creation case.
    /// ```text
    /// intrinsic proc sys_create(filename: &CStr) -> u32;
    /// ```
    Create: "sys_create",
    /// Intrinsic for the [`read`](https://man7.org/linux/man-pages/man2/read.2.html) system call.
    /// ```text
    /// intrinsic proc sys_read(fd: u32, count: u32, ghost buf: [u8; count], p: &sn buf) -> u32;
    /// ```
    Read: "sys_read",
    /// Intrinsic for the [`write`](https://man7.org/linux/man-pages/man2/write.2.html) system call.
    /// ```text
    /// intrinsic proc sys_write(fd: u32, count: u32, ghost mut buf: [u8; count], p: &sn buf) -> u32;
    /// ```
    Write: "sys_write",
    /// Intrinsic for the [`fstat`](https://man7.org/linux/man-pages/man2/fstat.2.html) system call.
    /// ```text
    /// intrinsic proc sys_fstat(fd: u32, ghost mut buf: Stat, p: &sn buf) -> u32;
    /// ```
    FStat: "sys_fstat",
    /// Intrinsic for the [`mmap`](https://man7.org/linux/man-pages/man2/mmap.2.html) system call,
    /// in the file-mapping case.
    /// ```text
    /// intrinsic proc sys_mmap(len: u64, prot: u32, fd: u32) -> u64;
    /// ```
    MMap: "sys_mmap",
    /// Intrinsic for the [`mmap`](https://man7.org/linux/man-pages/man2/mmap.2.html) system call,
    /// in the anonymous case.
    /// ```text
    /// intrinsic proc sys_mmap_anon(len: u64, prot: u32) -> u64;
    /// ```
    MMapAnon: "sys_mmap_anon",
    /// Intrinsic for `strlen`.
    /// ```text
    /// intrinsic proc strlen(s: &CStr) -> u64;
    /// ```
    Strlen: "strlen",
  }

  /// Intrinsic global variables.
  enum IntrinsicGlobal {}

  /// Intrinsic constants.
  enum IntrinsicConst {}

  /// Intrinsic typedefs.
  enum IntrinsicType {
    /// A C-style string: a zero-terminated array of characters with unknown length.
    /// ```text
    /// intrinsic struct CStr {
    ///   ghost len: nat,
    ///   buf: [u8; len + 1],
    ///   eq0: all i, buf[i] = some 0 <-> i = len
    /// }
    /// ```
    CStr: "CStr",
    /// The buffer filled by `fstat`.
    /// ```text
    /// intrinsic struct Stat {
    ///   st_dev: u64,
    ///   st_ino: u64,
    ///   st_nlink: u64,
    ///   st_mode: u32,
    ///   st_uid: u32,
    ///   st_gid: u32,
    ///   _pad: i32,
    ///   st_rdev: u64,
    ///   st_size: i64,
    ///   st_blksize: i64,
    ///   st_blocks: i64,
    ///   st_atime: i64,
    ///   st_atime_nsec: i64,
    ///   st_mtime: i64,
    ///   st_mtime_nsec: i64,
    ///   st_ctime: i64,
    ///   st_ctime_nsec: i64,
    /// }
    /// ```
    Stat: "Stat",
  }
}

/// The typechecking status of a typedef.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "memory", derive(DeepSizeOf))]
pub enum TypeTc {
  /// We have determined that this is a type but we have not yet examined the body.
  ForwardDeclared,
  /// We have the type of the type constructor.
  Typed(TypeTy)
}

impl TypeTc {
  /// Get the type of the typedef, if it has been deduced.
  #[must_use] pub fn ty(&self) -> Option<&TypeTy> {
    match self {
      TypeTc::ForwardDeclared => None,
      TypeTc::Typed(ty) => Some(ty)
    }
  }
}

/// An entity representing a type.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "memory", derive(DeepSizeOf))]
#[allow(variant_size_differences)]
pub struct TypeTy {
  /// If this is an intrinsic, this gives the intrinsic identifier.
  pub intrinsic: Option<IntrinsicType>,
  /// The number of type arguments (not included in `args`). There are no higher
  /// order types so this is essentially just `{A : *} {B : *} ...` with `tyargs`
  /// variables (named by their index).
  pub tyargs: u32,
  /// The non-type arguments to the type constructor.
  pub args: Box<[global::Arg]>,
  /// The value of the definition.
  pub val: global::Ty,
}

/// The typechecking status of a procedure.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "memory", derive(DeepSizeOf))]
pub enum ProcTc {
  /// We have determined that this is a procedure but we have not yet examined the body.
  ForwardDeclared,
  /// We have determined the type of the procedure.
  Typed(ProcTy),
}

impl ProcTc {
  /// Get the type of the procedure, if it has been deduced.
  #[must_use] pub fn ty(&self) -> Option<&ProcTy> {
    match self {
      ProcTc::ForwardDeclared => None,
      ProcTc::Typed(ty) => Some(ty)
    }
  }
}

/// The type of a procedure.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "memory", derive(DeepSizeOf))]
pub struct ProcTy {
  /// The kind of the procedure (`func`, `proc`, `main`)
  pub kind: ast::ProcKind,
  /// If this is an intrinsic, this gives the intrinsic identifier.
  pub intrinsic: Option<IntrinsicProc>,
  /// The number of type arguments (not included in `args`). There are no higher
  /// order types so this is essentially just `{A : *} {B : *} ...` with `tyargs`
  /// variables (named by their index).
  pub tyargs: u32,
  /// The non-type input arguments to the procedure.
  pub args: Box<[global::Arg]>,
  /// The out parameter origin variables. `outs.len() <= rets.len()` and the first
  /// `outs.len()` arguments in `rets` correspond to the out arguments.
  pub outs: Box<[u32]>,
  /// The output parameters and return values of the procedure.
  /// The first `outs.len()` elements are the output parameters.
  pub rets: Box<[global::Arg]>,
  /// The variant, a measure that decreases on recursive calls.
  pub variant: Option<global::Variant>,
}

/// The typechecking status of a global variable.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "memory", derive(DeepSizeOf))]
pub enum GlobalTc {
  /// We know this is a global but have not typechecked the body.
  ForwardDeclared,
  /// A user global that has been typechecked, to an expression with the given type.
  Checked(global::Ty),
}

/// The typechecking status of a constant.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "memory", derive(DeepSizeOf))]
pub enum ConstTc {
  /// We know this is a const but have not typechecked the body.
  ForwardDeclared,
  /// A user type that has been typechecked, with the original span,
  /// the (internal) declaration name, and the compiled value expression.
  Checked {
    /// The type of the constant
    ty: global::Ty,
    /// The value of the constant
    e: global::Expr,
    /// The constant after weak head normalization, precomputed for convenience
    whnf: global::Expr,
  }
}

/// A primitive type, operation, or proposition. Some keywords appear in multiple classes.
#[derive(Copy, Clone, Debug, Default)]
pub struct Prim {
  /// The primitive type record, if applicable.
  pub ty: Option<PrimType>,
  /// The primitive operation record, if applicable.
  pub op: Option<PrimOp>,
}
#[cfg(feature = "memory")] mm0_deepsize::deep_size_0!(Prim);

/// An operator, function, or type. These all live in one namespace so user types and
// functions cannot name-overlap.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "memory", derive(DeepSizeOf))]
#[allow(variant_size_differences)]
pub enum Entity {
  /// A primitive type, operation, or proposition. Some keywords appear in multiple classes.
  Prim(Prim),
  /// A named typedef.
  Type(Spanned<TypeTc>),
  /// A named operator/procedure/function.
  Proc(Spanned<ProcTc>),
  /// A named global variable.
  Global(Spanned<GlobalTc>),
  /// A named constant.
  Const(Spanned<ConstTc>),
}

impl Entity {
  /// Get the place where the entity was defined, if it is not a primitive.
  #[must_use] pub fn span(&self) -> Option<&FileSpan> {
    match self {
      Entity::Prim(_) => None,
      Entity::Type(Spanned {span, ..}) |
      Entity::Proc(Spanned {span, ..}) |
      Entity::Global(Spanned {span, ..}) |
      Entity::Const(Spanned {span, ..}) => Some(span)
    }
  }
}
// impl TuplePattern {
//   fn on_names<E>(&self, f: &mut impl FnMut(bool, Symbol, &Option<FileSpan>) -> Result<(), E>) -> Result<(), E> {
//     match self {
//       &TuplePattern::Name(ghost, n, ref sp) => if n != Symbol::UNDER { f(ghost, n, sp)? },
//       TuplePattern::Typed(p, _) => p.on_names(f)?,
//       TuplePattern::Tuple(ps, _) => for p in &**ps { p.on_names(f)? }
//       TuplePattern::Ready(_) => unreachable!("for unelaborated tuple patterns"),
//     }
//     Ok(())
//   }
// }

impl<C> Compiler<C> {
  /// Construct the initial list of primitive entities.
  pub fn make_names(i: &mut Interner) -> HashMap<Symbol, Entity> {
    fn get(names: &mut HashMap<Symbol, Entity>, a: Symbol) -> &mut Prim {
      let e = names.entry(a).or_insert_with(|| Entity::Prim(Prim::default()));
      if let Entity::Prim(p) = e {p} else {unreachable!()}
    }
    let mut names = HashMap::new();
    PrimType::scan(|p, s| get(&mut names, i.intern(s)).ty = Some(p));
    PrimOp::scan(|p, s| get(&mut names, i.intern(s)).op = Some(p));
    names
  }
}
