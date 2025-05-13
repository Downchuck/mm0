//! Build the mid-level IR from HIR

use std::{rc::Rc, fmt::Debug, mem};
use std::collections::{HashMap, hash_map::Entry};
use smallvec::SmallVec;
use if_chain::if_chain;
#[cfg(feature = "memory")] use mm0_deepsize_derive::DeepSizeOf;
use mm0_util::{u32_as_usize, FileSpan};
use crate::{Idx, Symbol};
use super::types;
use types::{IntTy, Size, Spanned, VarId as HVarId, hir, ty, mir};
use hir::GenId;
use ty::{TuplePattern, TuplePatternKind, TupleMatchKind};
#[allow(clippy::wildcard_imports)] use mir::*;

#[derive(Debug)]
struct GenMap {
  dominator: GenId,
  value: HashMap<HVarId, VarId>,
  cache: HashMap<HVarId, VarId>,
}

type TrMap<K, V> = HashMap<K, Result<V, HashMap<GenId, V>>>;
#[derive(Debug, Default)]
struct Translator<'a, 'n> {
  mvars: Option<&'n mut crate::infer::MVars<'a>>,
  tys: TrMap<ty::Ty<'a>, Ty>,
  exprs: TrMap<ty::Expr<'a>, Expr>,
  places: TrMap<ty::Place<'a>, EPlace>,
  gen_vars: HashMap<GenId, GenMap>,
  locations: HashMap<HVarId, VarId>,
  /// Some variables are replaced by places when referenced; this keeps track of them.
  vars: HashMap<VarId, (EPlace, Place)>,
  located: HashMap<VarId, Vec<VarId>>,
  next_var: VarId,
  cur_gen: GenId,
  subst: HashMap<HVarId, Expr>,
}

trait Translate<'a> {
  type Output;
  fn tr(self, _: &mut Translator<'a, '_>) -> Self::Output;
}
trait TranslateBase<'a>: Sized {
  type Output;
  fn get_mut<'b>(_: &'b mut Translator<'a, '_>) ->
    &'b mut TrMap<&'a ty::WithMeta<Self>, Rc<Self::Output>>;
  fn make(&'a self, tr: &mut Translator<'a, '_>) -> Rc<Self::Output>;
}

impl<'a> TranslateBase<'a> for ty::TyKind<'a> {
  type Output = TyKind;
  fn get_mut<'b>(t: &'b mut Translator<'a, '_>) -> &'b mut TrMap<ty::Ty<'a>, Ty> { &mut t.tys }
  fn make(&'a self, tr: &mut Translator<'a, '_>) -> Ty {
    Rc::new(match *self {
      ty::TyKind::Unit => TyKind::Unit,
      ty::TyKind::True => TyKind::True,
      ty::TyKind::False => TyKind::False,
      ty::TyKind::Bool => TyKind::Bool,
      ty::TyKind::Var(v) => TyKind::Var(v),
      ty::TyKind::Int(ity) => TyKind::Int(ity),
      ty::TyKind::Array(ty, n) => TyKind::Array(ty.tr(tr), n.tr(tr)),
      ty::TyKind::Own(ty) => TyKind::Own(ty.tr(tr)),
      ty::TyKind::Shr(lft, ty) => TyKind::Shr(lft.tr(tr), ty.tr(tr)),
      ty::TyKind::Ref(lft, ty) => TyKind::Ref(lft.tr(tr), ty.tr(tr)),
      ty::TyKind::RefSn(e) => TyKind::RefSn(e.tr(tr)),
      ty::TyKind::List(tys) => tr.tr_list(tys),
      ty::TyKind::Sn(a, ty) => TyKind::Sn(a.tr(tr), ty.tr(tr)),
      ty::TyKind::Struct(args) => tr.tr_struct(args),
      ty::TyKind::All(pat, ty) => tr.tr_all(pat, ty),
      ty::TyKind::Imp(p, q) => TyKind::Imp(p.tr(tr), q.tr(tr)),
      ty::TyKind::Wand(p, q) => TyKind::Wand(p.tr(tr), q.tr(tr)),
      ty::TyKind::Not(p) => TyKind::Not(p.tr(tr)),
      ty::TyKind::And(ps) => TyKind::And(ps.tr(tr)),
      ty::TyKind::Or(ps) => TyKind::Or(ps.tr(tr)),
      ty::TyKind::If(c, t, e) => TyKind::If(c.tr(tr), t.tr(tr), e.tr(tr)),
      ty::TyKind::Ghost(ty) => TyKind::Ghost(ty.tr(tr)),
      ty::TyKind::Uninit(ty) => TyKind::Uninit(ty.tr(tr)),
      ty::TyKind::Pure(e) => TyKind::Pure(e.tr(tr)),
      ty::TyKind::User(f, tys, es) => TyKind::User(f, tys.tr(tr), es.tr(tr)),
      ty::TyKind::Heap(e, v, ty) => TyKind::Heap(e.tr(tr), v.tr(tr), ty.tr(tr)),
      ty::TyKind::HasTy(e, ty) => TyKind::HasTy(e.tr(tr), ty.tr(tr)),
      ty::TyKind::Input => TyKind::Input,
      ty::TyKind::Output => TyKind::Output,
      ty::TyKind::Moved(ty) => TyKind::Moved(ty.tr(tr)),
      ty::TyKind::Infer(v) => match tr.mvars.as_mut().expect("no inference context").ty.lookup(v) {
        Some(ty) => return ty.tr(tr),
        None => panic!("uninferred type variable {v:?}"),
      }
      ty::TyKind::Error => panic!("unreachable: {self:?}"),
    })
  }
}

impl<'a> TranslateBase<'a> for ty::PlaceKind<'a> {
  type Output = EPlaceKind;
  fn get_mut<'b>(t: &'b mut Translator<'a, '_>) -> &'b mut TrMap<ty::Place<'a>, EPlace> { &mut t.places }
  fn make(&'a self, tr: &mut Translator<'a, '_>) -> EPlace {
    Rc::new(match *self {
      ty::PlaceKind::Var(v) => {
        let v = tr.location(v);
        match tr.vars.get(&v) {
          Some((p, _)) => return p.clone(),
          None => EPlaceKind::Var(v)
        }
      }
      ty::PlaceKind::Index(a, ty, i) => EPlaceKind::Index(a.tr(tr), ty.tr(tr), i.tr(tr)),
      ty::PlaceKind::Slice(a, ty, [i, l]) =>
        EPlaceKind::Slice(a.tr(tr), ty.tr(tr), [i.tr(tr), l.tr(tr)]),
      ty::PlaceKind::Proj(a, ty, i) => EPlaceKind::Proj(a.tr(tr), ty.tr(tr), i),
      ty::PlaceKind::Error => unreachable!(),
    })
  }
}

impl<'a> TranslateBase<'a> for ty::ExprKind<'a> {
  type Output = ExprKind;
  fn get_mut<'b>(t: &'b mut Translator<'a, '_>) -> &'b mut TrMap<ty::Expr<'a>, Expr> { &mut t.exprs }
  fn make(&'a self, tr: &mut Translator<'a, '_>) -> Expr {
    Rc::new(match *self {
      ty::ExprKind::Unit => ExprKind::Unit,
      ty::ExprKind::Var(v) => {
        let v = tr.location(v);
        match tr.vars.get(&v) {
          Some((p, _)) => return p.to_expr(),
          None => ExprKind::Var(v),
        }
      }
      ty::ExprKind::Const(c) => ExprKind::Const(c),
      ty::ExprKind::Bool(b) => ExprKind::Bool(b),
      ty::ExprKind::Int(n) => ExprKind::Int(n.clone()),
      ty::ExprKind::Unop(op, e) => ExprKind::Unop(op, e.tr(tr)),
      ty::ExprKind::Binop(op, e1, e2) => ExprKind::Binop(op, e1.tr(tr), e2.tr(tr)),
      ty::ExprKind::Index(a, i) => ExprKind::Index(a.tr(tr), i.tr(tr)),
      ty::ExprKind::Slice([a, i, l]) => ExprKind::Slice(a.tr(tr), i.tr(tr), l.tr(tr)),
      ty::ExprKind::Proj(a, i) => ExprKind::Proj(a.tr(tr), i),
      ty::ExprKind::UpdateIndex([a, i, v]) => ExprKind::UpdateIndex(a.tr(tr), i.tr(tr), v.tr(tr)),
      ty::ExprKind::UpdateSlice([a, i, l, v]) =>
        ExprKind::UpdateSlice(a.tr(tr), i.tr(tr), l.tr(tr), v.tr(tr)),
      ty::ExprKind::UpdateProj(a, i, v) => ExprKind::UpdateProj(a.tr(tr), i, v.tr(tr)),
      ty::ExprKind::List(es) => ExprKind::List(es.tr(tr)),
      ty::ExprKind::Array(es) => ExprKind::Array(es.tr(tr)),
      ty::ExprKind::Sizeof(ty) => ExprKind::Sizeof(ty.tr(tr)),
      ty::ExprKind::Ref(e) => ExprKind::Ref(e.tr(tr)),
      ty::ExprKind::Mm0(ref e) => ExprKind::Mm0(e.tr(tr)),
      ty::ExprKind::Call {f, tys, args} => ExprKind::Call {f, tys: tys.tr(tr), args: args.tr(tr)},
      ty::ExprKind::If {cond, then, els} =>
        ExprKind::If {cond: cond.tr(tr), then: then.tr(tr), els: els.tr(tr)},
      ty::ExprKind::Infer(v) => match tr.mvars.as_mut().expect("no inference context").expr.lookup(v) {
        Some(e) => return e.tr(tr),
        None => panic!("uninferred expr variable {v:?}"),
      }
      ty::ExprKind::Error => unreachable!(),
    })
  }
}

impl<'a, T: TranslateBase<'a>> Translate<'a> for &'a ty::WithMeta<T> {
  type Output = Rc<T::Output>;
  fn tr(self, tr: &mut Translator<'a, '_>) -> Rc<T::Output> {
    if tr.subst.is_empty() {
      let gen_ = tr.cur_gen;
      if let Some(v) = T::get_mut(tr).get(self) {
        match v {
          Ok(r) => return r.clone(),
          Err(m) => if let Some(r) = m.get(&gen_) { return r.clone() }
        }
      }
      let r = T::make(&self.k, tr);
      match T::get_mut(tr).entry(self) {
        Entry::Occupied(mut e) => match e.get_mut() {
          Ok(_) => unreachable!(),
          Err(m) => { m.insert(gen_, r.clone()); }
        }
        Entry::Vacant(e) => {
          e.insert(if self.flags.contains(ty::Flags::HAS_VAR) {
            let mut m = HashMap::new();
            m.insert(gen_, r.clone());
            Err(m)
          } else {
            Ok(r.clone())
          });
        }
      }
      r
    } else {
      T::make(&self.k, tr)
    }
  }
}

impl<'a, T: Translate<'a> + Copy> Translate<'a> for &'a [T] {
  type Output = Box<[T::Output]>;
  fn tr(self, tr: &mut Translator<'a, '_>) -> Box<[T::Output]> {
    self.iter().map(|&e| e.tr(tr)).collect()
  }
}

impl<'a, T: Translate<'a>> Translate<'a> for hir::Spanned<'a, T> {
  type Output = hir::Spanned<'a, T::Output>;
  fn tr(self, tr: &mut Translator<'a, '_>) -> hir::Spanned<'a, T::Output> {
    self.map_into(|v| v.tr(tr))
  }
}

impl<'a> Translate<'a> for ty::ExprTy<'a> {
  type Output = ExprTy;
  fn tr(self, tr: &mut Translator<'a, '_>) -> Self::Output {
    (self.0.map(|e| e.tr(tr)), self.1.tr(tr))
  }
}

impl<'a> Translate<'a> for &'a ty::Mm0Expr<'a> {
  type Output = Mm0Expr;
  fn tr(self, tr: &mut Translator<'a, '_>) -> Mm0Expr {
    Mm0Expr { subst: self.subst.tr(tr), expr: self.expr }
  }
}

impl<'a> Translate<'a> for HVarId {
  type Output = VarId;
  fn tr(self, tr: &mut Translator<'a, '_>) -> VarId {
    tr.tr_var(self, tr.cur_gen)
  }
}

impl<'a> Translate<'a> for PreVar {
  type Output = VarId;
  fn tr(self, tr: &mut Translator<'a, '_>) -> VarId {
    match self {
      PreVar::Ok(v) => v,
      PreVar::Pre(v) => v.tr(tr),
      PreVar::Fresh => tr.fresh_var(),
    }
  }
}

impl<'a> Translate<'a> for ty::Lifetime {
  type Output = Lifetime;
  fn tr(self, tr: &mut Translator<'a, '_>) -> Lifetime {
    match self {
      ty::Lifetime::Infer(_) | // FIXME
      ty::Lifetime::Extern => Lifetime::Extern,
      ty::Lifetime::Place(v) => Lifetime::Place(v.tr(tr)),
    }
  }
}

impl ty::TyS<'_> {
  fn is_unit_dest(&self) -> bool {
    matches!(self.k,
      ty::TyKind::Unit |
      ty::TyKind::True |
      ty::TyKind::Pure(&ty::WithMeta {k: ty::ExprKind::Bool(true), ..}) |
      ty::TyKind::Uninit(_))
  }
}

impl<'a> Translator<'a, '_> {
  #[must_use] fn fresh_var(&mut self) -> VarId { self.next_var.fresh() }

  fn tr_var(&mut self, v: HVarId, gen_: GenId) -> VarId {
    let gm = self.gen_vars.get(&gen_).expect("unknown generation");
    if let Some(&val) = gm.cache.get(&v) { return val }
    let val =
      if let Some(&val) = gm.value.get(&v) { val }
      else if gen_ == GenId::ROOT { self.next_var.fresh() }
      else { let dom = gm.dominator; self.tr_var(v, dom) };
    self.gen_vars.get_mut(&gen_).expect("unknown generation").cache.insert(v, val);
    match self.locations.entry(v) {
      Entry::Occupied(e) => { let root = *e.get(); self.locate(root).push(val) }
      Entry::Vacant(e) => { e.insert(val); }
    }
    val
  }

  fn location(&mut self, var: HVarId) -> VarId {
    *self.locations.entry(var).or_insert_with(|| self.next_var.fresh())
  }

  fn locate(&mut self, var: VarId) -> &mut Vec<VarId> {
    self.located.entry(var).or_default()
  }

  fn add_gen(&mut self, dominator: GenId, gen_: GenId, value: HashMap<HVarId, VarId>) {
    assert!(self.gen_vars.insert(gen_,
      GenMap { dominator, value, cache: Default::default() }).is_none())
  }

  fn try_add_gen(&mut self, dominator: GenId, gen_: GenId) {
    if let Entry::Vacant(e) = self.gen_vars.entry(gen_) {
      e.insert(GenMap { dominator, value: Default::default(), cache: Default::default() });
    }
  }

  fn with_gen<R>(&mut self, mut gen_: GenId, f: impl FnOnce(&mut Self) -> R) -> R {
    mem::swap(&mut self.cur_gen, &mut gen_);
    let r = f(self);
    self.cur_gen = gen_;
    r
  }

  fn tr_tup_pat(&mut self, pat: ty::TuplePattern<'a>, e: Expr) {
    assert!(self.subst.insert(pat.k.var, e.clone()).is_none());
    match pat.k.k {
      TuplePatternKind::Name(_) => {}
      TuplePatternKind::Tuple(pats, mk) => match mk {
        TupleMatchKind::Unit | TupleMatchKind::True => {}
        TupleMatchKind::List | TupleMatchKind::Struct =>
          for (i, &pat) in pats.iter().enumerate() {
            self.tr_tup_pat(pat,
              Rc::new(ExprKind::Proj(e.clone(), i.try_into().expect("overflow"))))
          }
        TupleMatchKind::Array =>
          for (i, &pat) in pats.iter().enumerate() {
            self.tr_tup_pat(pat,
              Rc::new(ExprKind::Index(e.clone(), Rc::new(ExprKind::Int(i.into())))))
          }
        TupleMatchKind::And => for pat in pats { self.tr_tup_pat(pat, e.clone()) }
        TupleMatchKind::Sn => {
          self.tr_tup_pat(pats[0], e);
          self.tr_tup_pat(pats[1], Rc::new(ExprKind::Unit));
        }
        TupleMatchKind::Own |
        TupleMatchKind::Shr => panic!("existential pattern match in proof relevant position")
      }
      TuplePatternKind::Error(_) => unreachable!()
    }
  }
  fn finish_tup_pat(&mut self, pat: ty::TuplePattern<'a>) {
    match pat.k.k {
      TuplePatternKind::Name(_) => {}
      TuplePatternKind::Tuple(pats, _) =>
        for pat in pats.iter().rev() { self.finish_tup_pat(pat) }
      TuplePatternKind::Error(_) => unreachable!()
    }
    self.subst.remove(&pat.k.var);
  }

  fn tr_all(&mut self, pat: ty::TuplePattern<'a>, ty: ty::Ty<'a>) -> TyKind {
    let v = pat.k.var.tr(self);
    let tgt = pat.k.ty.tr(self);
    if pat.k.k.is_name() {
      TyKind::All(v, tgt, ty.tr(self))
    } else {
      self.tr_tup_pat(pat, Rc::new(ExprKind::Var(v)));
      let ty = ty.tr(self);
      self.finish_tup_pat(pat);
      TyKind::All(v, tgt, ty)
    }
  }

  fn tr_list(&mut self, tys: &'a [ty::Ty<'a>]) -> TyKind {
    TyKind::Struct(tys.iter().map(|&ty| {
      let attr = ArgAttr::NONDEP | ArgAttr::ghost(ty.ghostly());
      let ty = ty.tr(self);
      Arg {attr, var: self.fresh_var(), ty}
    }).collect())
  }

  fn tr_struct(&mut self, args: &'a [ty::Arg<'a>]) -> TyKind {
    let mut args2 = Vec::with_capacity(args.len());
    for &arg in args {
      match arg.k {
        (attr, ty::ArgKind::Lam(pat)) => {
          let mut attr2 = ArgAttr::empty();
          if attr.contains(ty::ArgAttr::NONDEP) { attr2 |= ArgAttr::NONDEP }
          if pat.k.ty.ghostly() { attr2 |= ArgAttr::GHOST }
          let v = pat.k.var.tr(self);
          let ty = pat.k.ty.tr(self);
          if !pat.k.k.is_name() {
            self.tr_tup_pat(pat, Rc::new(ExprKind::Var(v)));
          }
          args2.push(Arg {attr: attr2, var: v, ty})
        }
        (_, ty::ArgKind::Let(pat, e)) => {
          let e = e.tr(self);
          self.tr_tup_pat(pat, e)
        }
      }
    }
    for &arg in args {
      if !matches!(arg.k.1, ty::ArgKind::Lam(pat) if pat.k.k.is_name()) {
        self.finish_tup_pat(arg.k.1.var())
      }
    }
    TyKind::Struct(args2.into_boxed_slice())
  }
}

/// A `PreVar` is used as the destination for rvalues, because sometimes we don't know in advance
/// the generation as of the end of evaluation, when the destination variable is actually filled.
/// We need to know the generation to translate [`HVarId`] into [`VarId`]. But in some cases we
/// don't have a variable in the source and just need a fresh variable, in which case we can use
/// `Ok` or `Fresh` to create an unused fresh variable, or a variable that was generated beforehand.
#[derive(Copy, Clone, Debug)]
enum PreVar {
  /// Use this variable
  Ok(VarId),
  /// Translate this variable at the current generation
  Pre(HVarId),
  /// Make up some fresh variable
  Fresh,
}

/// A destination designator for expressions that are to be placed in a memory location.
/// See [`PreVar`].
type Dest<'a> = Option<hir::Spanned<'a, PreVar>>;

/// A variant on `Dest` for values that are going out of a block via `break`.
type BlockDest<'a> = Option<(hir::Spanned<'a, VarId>, ty::ExprTy<'a>)>;

/// A `JoinBlock` represents a potential jump location, together with the information needed to
/// correctly pass all the updated values of mutable variables from the current context.
/// * `gen_`: The generation on entry to the target
/// * `muts`: The variables that could potentially have been mutated between when this `JoinBlock`
///   was created and the context we are jumping from. These lists are calculated during type
///   inference and are mostly syntax directed.
type JoinPoint = (GenId, Rc<[HVarId]>);

/// A `JoinBlock` represents a potential jump location, together with the information needed to
/// correctly pass all the updated values of mutable variables from the current context.
#[derive(Clone, Debug)]
struct JoinBlock(BlockId, JoinPoint);

/// Data to support the `(jump label[i])` operation.
type LabelData = (BlockId, Rc<[(VarId, bool)]>);

#[derive(Debug)]
struct LabelGroupData<'a> {
  /// This is `Some(((gen_, muts), labs))` if jumping to this label is valid. `gen_, muts` are
  /// parameters to the [`JoinBlock`] (which are shared between all labels), and
  /// `labs[i] = (tgt, args)` give the target block and the list of variable names to pass
  jumps: Option<(JoinPoint, Rc<[LabelData]>)>,
  /// The [`JoinBlock`] for breaking to this label, as well as a `BlockDest` which receives the
  /// `break e` expression.
  brk: Option<(JoinBlock, BlockDest<'a>)>,
}

#[derive(Default, Debug)]
struct BlockTreeBuilder {
  groups: Vec<(Vec<BlockTree>, SmallVec<[BlockId; 2]>)>,
  ordered: Vec<BlockTree>,
}

impl BlockTreeBuilder {
  fn push(&mut self, bl: BlockId) { self.ordered.push(BlockTree::One(bl)) }
  fn push_group(&mut self, group: SmallVec<[BlockId; 2]>) {
    self.groups.push((std::mem::take(&mut self.ordered), group))
  }

  fn pop(&mut self) {
    let (many, group) = self.groups.pop().expect("unbalanced stack");
    let group = Box::new((group, std::mem::replace(&mut self.ordered, many)));
    self.ordered.push(BlockTree::LabelGroup(group));
  }

  fn truncate(&mut self, n: usize) {
    for _ in n..self.groups.len() { self.pop() }
  }

  fn append_to(&mut self, bt: &mut BlockTree) {
    assert!(self.groups.is_empty(), "unbalanced stack");
    if !self.ordered.is_empty() {
      if let BlockTree::Many(vec) = bt {
        vec.append(&mut self.ordered)
      } else {
        self.ordered.insert(0, std::mem::take(bt));
        *bt = BlockTree::Many(std::mem::take(&mut self.ordered));
      }
    }
  }
}

/// The global initializer, which contains let bindings for every global variable.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "memory", derive(DeepSizeOf))]
pub(crate) struct Initializer {
  cfg: Cfg,
  /// A list of allocated globals and the variables they were assigned to.
  globals: Vec<(Symbol, bool, VarId, Ty)>,
  cur: Block<(BlockId, CtxId, GenId)>,
}

impl Default for Initializer {
  fn default() -> Self {
    let mut cfg = Cfg::default();
    let cur = Ok((cfg.new_block(CtxId::ROOT, 0), CtxId::ROOT, GenId::ROOT));
    Self {cfg, cur, globals: vec![]}
  }
}

/// There is no block ID because [`Return`](Terminator::Return) doesn't jump to a block.
#[derive(Debug)]
struct Returns {
  outs: Box<[HVarId]>,
  /// The names of the return places.
  args: Box<[(VarId, bool)]>,
}

/// The main context struct for the MIR builder.
#[derive(Debug)]
pub(crate) struct BuildMir<'a, 'n> {
  /// The main data structure, the MIR control flow graph
  cfg: Cfg,
  /// Contains the current generation and other information relevant to the [`tr`](Self::tr)
  /// function
  tr: Translator<'a, 'n>,
  /// The stack of labels in scope
  labels: Vec<(HVarId, LabelGroupData<'a>)>,
  /// The in-progress parts of the `BlockTree`
  tree: BlockTreeBuilder,
  /// If this is `Some(_)` then returning is possible at this point.
  returns: Option<Rc<Returns>>,
  /// A list of allocated globals and the variables they were assigned to.
  globals: Vec<(Symbol, bool, VarId, Ty)>,
  /// The current block, which is where new statements from functions like [`Self::expr()`] are
  /// inserted.
  cur_block: BlockId,
  /// The current context, which contains typing information about the variables that are in scope.
  cur_ctx: CtxId,
}

/// Indicates that construction diverged. See [`Block`].
#[derive(Copy, Clone, Debug)]
#[cfg_attr(feature = "memory", derive(DeepSizeOf))]
pub(crate) struct Diverged;

/// This is the return type of functions that construct a `T` but may choose instead to perform
/// some kind of non-local exit, in which case [`cur_block`](BuildMir::cur_block) will be
/// terminated.
pub(crate) type Block<T> = Result<T, Diverged>;

impl<'a, 'n> BuildMir<'a, 'n> {
  pub(crate) fn new(mvars: Option<&'n mut crate::infer::MVars<'a>>) -> Self {
    let mut tr = Translator {
      mvars,
      next_var: VarId::default(),
      cur_gen: GenId::ROOT,
      ..Default::default()
    };
    tr.try_add_gen(GenId::ROOT, GenId::ROOT);
    Self {
      cfg: Cfg::default(),
      labels: vec![],
      tree: Default::default(),
      returns: None,
      tr,
      globals: vec![],
      cur_block: BlockId::ENTRY,
      cur_ctx: CtxId::ROOT,
    }
  }

  fn fresh_var(&mut self) -> VarId { self.tr.fresh_var() }
  fn fresh_var_span(&mut self, span: FileSpan) -> Spanned<VarId> {
    Spanned { span, k: self.fresh_var() }
  }

  #[inline] fn cur(&self) -> (BlockId, CtxId, GenId) {
    (self.cur_block, self.cur_ctx, self.tr.cur_gen)
  }

  #[inline] fn set(&mut self, (block, ctx, gen_): (BlockId, CtxId, GenId)) {
    self.cur_block = block; self.cur_ctx = ctx; self.tr.cur_gen = gen_;
  }

  fn cur_block(&mut self) -> &mut BasicBlock { &mut self.cfg[self.cur_block] }

  fn new_block(&mut self, parent: usize) -> BlockId {
    self.cfg.new_block(self.cur_ctx, parent)
  }

  fn dominated_block(&mut self, ctx: CtxId) -> BlockId {
    let n = self.cfg.ctxs.len(ctx);
    let bl = self.new_block(n);
    self.cur_block().stmts.push(Statement::DominatedBlock(bl, ctx));
    bl
  }

  fn extend_ctx(&mut self, var: Spanned<VarId>, r: bool, ty: ExprTy) {
    self.cur_ctx = self.cfg.ctxs.extend(self.cur_ctx, var, r, ty)
  }

  fn push_stmt(&mut self, stmt: Statement) {
    match &stmt {
      Statement::Let(LetKind::Let(v, e), r, ty, _) =>
        self.extend_ctx(v.clone(), *r, (e.clone(), ty.clone())),
      Statement::Let(LetKind::Ptr([(v, ty), (h, ty2)]), hr, _, _) => {
        self.extend_ctx(v.clone(), false, (None, ty.clone()));
        self.extend_ctx(h.clone(), *hr, (Some(Rc::new(ExprKind::Unit)), ty2.clone()));
      }
      Statement::Assign(_, _, _, vars) => for v in &**vars {
        self.extend_ctx(v.to.clone(), v.rel, v.ety.clone())
      }
      Statement::LabelGroup(..) | Statement::PopLabelGroup | Statement::DominatedBlock(..) => {}
    }
    self.cur_block().stmts.push(stmt);
  }

  fn tr<T: Translate<'a>>(&mut self, t: T) -> T::Output { t.tr(&mut self.tr) }

  fn tr_gen<T: Translate<'a>>(&mut self, t: T, gen_: GenId) -> T::Output {
    self.tr.with_gen(gen_, |tr| t.tr(tr))
  }

  fn as_temp(&mut self, e: hir::Expr<'a>) -> Block<VarId> {
    let v = self.fresh_var();
    let dest = hir::Spanned { span: e.span, k: PreVar::Ok(v) };
    self.expr(e, Some(dest))?;
    Ok(v)
  }

  fn assert(&mut self, span: FileSpan, v_cond: Operand, cond: Expr) -> VarId {
    let vh = self.fresh_var();
    let n = self.cfg.ctxs.len(self.cur_ctx);
    self.extend_ctx(Spanned { span, k: vh }, false, (None, Rc::new(TyKind::Pure(cond))));
    let tgt = self.new_block(n);
    self.cur_block().terminate(Terminator::Assert(v_cond, vh, tgt));
    self.cur_block = tgt;
    vh
  }

  fn index_projection(&mut self, span: &'a FileSpan,
    idx: hir::Expr<'a>, hyp_or_n: Result<hir::Expr<'a>, hir::Expr<'a>>
  ) -> Block<Projection> {
    let vi = self.as_temp(idx)?;
    Ok(Projection::Index(vi, match hyp_or_n {
      Ok(hyp) => self.as_temp(hyp)?,
      Err(n) => {
        let vn = self.as_temp(n)?;
        let vb_s = self.fresh_var_span(span.clone());
        let vb = vb_s.k;
        let cond = Rc::new(ExprKind::Binop(types::Binop::Lt,
          Rc::new(ExprKind::Var(vi)),
          Rc::new(ExprKind::Var(vn))));
        self.push_stmt(Statement::Let(
          LetKind::Let(vb_s, Some(cond.clone())), true, Rc::new(TyKind::Bool),
          RValue::Binop(Binop::Lt(IntTy::NAT),
            Operand::Copy(vi.into()), vn.into())));
        self.assert(span.clone(), vb.into(), cond)
      }
    }))
  }

  fn slice_projection(&mut self, span: &'a FileSpan,
    idx: hir::Expr<'a>, len: hir::Expr<'a>, hyp_or_n: Result<hir::Expr<'a>, hir::Expr<'a>>
  ) -> Block<Projection> {
    let vi = self.as_temp(idx)?;
    let vl = self.as_temp(len)?;
    Ok(Projection::Slice(vi, vl, match hyp_or_n {
      Ok(hyp) => self.as_temp(hyp)?,
      Err(n) => {
        let vn = self.as_temp(n)?;
        let v_add_s = self.fresh_var_span(span.clone());
        let v_add = v_add_s.k;
        let add = Rc::new(ExprKind::Binop(types::Binop::Add,
          Rc::new(ExprKind::Var(vi)),
          Rc::new(ExprKind::Var(vl))));
        self.push_stmt(Statement::Let(
          LetKind::Let(v_add_s, Some(add.clone())), true,
          Rc::new(TyKind::Int(IntTy::INT)),
          RValue::Binop(Binop::Add(IntTy::NAT),
            Operand::Copy(vi.into()), Operand::Copy(vl.into()))));
        let v_cond_s = self.fresh_var_span(span.clone());
        let v_cond = v_cond_s.k;
        let cond = Rc::new(ExprKind::Binop(types::Binop::Le,
          add, Rc::new(ExprKind::Var(vn))));
        self.push_stmt(Statement::Let(
          LetKind::Let(v_cond_s, Some(cond.clone())), true,
          Rc::new(TyKind::Bool),
          RValue::Binop(Binop::Le(IntTy::NAT), v_add.into(), vn.into())));
        self.assert(span.clone(), v_cond.into(), cond)
      }
    }))
  }

  fn place(&mut self, e: hir::Place<'a>) -> Block<Place> {
    Ok(match e.k.0 {
      hir::PlaceKind::Var(v) => {
        let v2 = self.tr.location(v);
        self.tr.vars.get(&v2).map_or_else(|| v2.into(), |p| p.1.clone())
      }
      hir::PlaceKind::Index(args) => {
        let (ty, arr, idx, hyp_or_n) = *args;
        self.place(arr)?.proj((self.tr(ty), self.index_projection(e.span, idx, hyp_or_n)?))
      }
      hir::PlaceKind::Slice(args) => {
        let (ty, arr, [idx, len], hyp_or_n) = *args;
        self.place(arr)?.proj((self.tr(ty), self.slice_projection(e.span, idx, len, hyp_or_n)?))
      }
      hir::PlaceKind::Proj(pk, e, i) => {
        self.place(e.1)?.proj((self.tr(e.0), Projection::Proj(pk.into(), i)))
      }
      hir::PlaceKind::Deref(e) =>
        Place::local(self.as_temp(e.1)?).proj((self.tr(e.0), Projection::Deref)),
      hir::PlaceKind::Error => unreachable!()
    })
  }

  fn ignore_place(&mut self, e: hir::Place<'a>) -> Block<()> {
    match e.k.0 {
      hir::PlaceKind::Var(_) => {}
      hir::PlaceKind::Index(args) => {
        let (_, arr, idx, hyp_or_n) = *args;
        self.ignore_place(arr)?;
        self.expr(idx, None)?;
        if let Ok(hyp) = hyp_or_n { self.expr(hyp, None)?; }
      }
      hir::PlaceKind::Slice(args) => {
        let (_, arr, [idx, len], hyp_or_n) = *args;
        self.ignore_place(arr)?;
        self.expr(idx, None)?;
        self.expr(len, None)?;
        if let Ok(hyp) = hyp_or_n { self.expr(hyp, None)?; }
      }
      hir::PlaceKind::Proj(_, e, _) => self.ignore_place(e.1)?,
      hir::PlaceKind::Deref(e) => self.expr(e.1, None)?,
      hir::PlaceKind::Error => unreachable!()
    }
    Ok(())
  }

  fn expr_place(&mut self, e: hir::Expr<'a>) -> Block<Place> {
    Ok(match e.k.0 {
      hir::ExprKind::Var(v, gen_) => {
        let v = self.tr_gen(v, gen_);
        self.tr.vars.get(&v).map_or_else(|| v.into(), |p| p.1.clone())
      }
      hir::ExprKind::Index(args) => {
        let (ty, [arr, idx], hyp_or_n) = *args;
        self.expr_place(arr)?.proj((self.tr(ty), self.index_projection(e.span, idx, hyp_or_n)?))
      }
      hir::ExprKind::Slice(args) => {
        let (ty, [arr, idx, len], hyp_or_n) = *args;
        self.expr_place(arr)?.proj((self.tr(ty), self.slice_projection(e.span, idx, len, hyp_or_n)?))
      }
      hir::ExprKind::Proj(pk, e, i) =>
        self.expr_place(e.1)?.proj((self.tr(e.0), Projection::Proj(pk.into(), i))),
      hir::ExprKind::Ref(e) => self.place(*e)?,
      hir::ExprKind::Deref(e) =>
        Place::local(self.as_temp(e.1)?).proj((self.tr(e.0), Projection::Deref)),
      _ => Place::local(self.as_temp(e)?)
    })
  }

  fn copy_or_move(&mut self, e: hir::Expr<'a>) -> Block<Operand> {
    let copy = e.ty().is_copy();
    let p = self.expr_place(e)?;
    Ok(if copy {Operand::Copy(p)} else {Operand::Move(p)})
  }

  fn copy_or_ref(&mut self, e: hir::Place<'a>) -> Block<Operand> {
    let copy = e.ty().is_copy();
    let p = self.place(e)?;
    Ok(if copy {Operand::Copy(p)} else {Operand::Ref(p)})
  }

  fn copy_or_move_place(&mut self, e: hir::Place<'a>) -> Block<Operand> {
    let copy = e.ty().is_copy();
    let p = self.place(e)?;
    Ok(if copy {Operand::Copy(p)} else {Operand::Move(p)})
  }

  fn operand(&mut self, e: hir::Expr<'a>) -> Block<Operand> {
    Ok(match e.k.0 {
      hir::ExprKind::Var(_, _) |
      hir::ExprKind::Index(_) |
      hir::ExprKind::Slice(_) |
      hir::ExprKind::Proj(_, _, _) |
      hir::ExprKind::Deref(_) => self.copy_or_move(e)?,
      hir::ExprKind::Rval(e) => self.copy_or_move(*e)?,
      hir::ExprKind::ArgRef(e) => self.copy_or_move_place(*e)?,
      hir::ExprKind::Ref(e) => self.copy_or_ref(*e)?,
      hir::ExprKind::Unit => Constant::unit().into(),
      hir::ExprKind::ITrue |
      hir::ExprKind::Assert { trivial: Some(true), .. } => Constant::itrue().into(),
      hir::ExprKind::Bool(b) => Constant::bool(b).into(),
      hir::ExprKind::Int(n) => {
        let ty::TyKind::Int(ity) = e.ty().k else { unreachable!() };
        Constant::int(ity, n.clone()).into()
      }
      hir::ExprKind::Const(a) => Constant {ety: self.tr(e.k.1), k: ConstKind::Const(a)}.into(),
      hir::ExprKind::Call(ref call)
      if matches!(call.rk, hir::ReturnKind::Unit | hir::ReturnKind::Unreachable) => {
        self.expr(e, None)?;
        Constant::unit().into()
      }
      _ => self.as_temp(e)?.into()
    })
  }

  fn rvalue(&mut self, e: hir::Expr<'a>) -> Block<RValue> {
    Ok(match e.k.0 {
      hir::ExprKind::Unop(op, e) => {
        let v = self.as_temp(*e)?;
        RValue::Unop(op, v.into())
      }
      hir::ExprKind::Binop(op, e1, e2) => {
        let v1 = self.as_temp(*e1)?;
        let v2 = self.as_temp(*e2)?;
        RValue::Binop(op, v1.into(), v2.into())
      }
      hir::ExprKind::Eq(ty, inv, e1, e2) => {
        let ty = self.tr(ty);
        let v1 = self.as_temp(*e1)?;
        let v2 = self.as_temp(*e2)?;
        RValue::Eq(ty, inv, v1.into(), v2.into())
      }
      hir::ExprKind::Sn(x, h) => {
        let vx = self.as_temp(*x)?;
        let vh = h.map(|h| self.as_temp(*h)).transpose()?.map(Into::into);
        RValue::Pun(PunKind::Sn(vh), vx.into())
      }
      hir::ExprKind::List(hir::ListKind::List | hir::ListKind::Struct, es) =>
        RValue::List(es.into_iter().map(|e| self.operand(e)).collect::<Block<_>>()?),
      hir::ExprKind::List(hir::ListKind::Array, es) =>
        RValue::Array(es.into_iter().map(|e| self.operand(e)).collect::<Block<_>>()?),
      hir::ExprKind::List(hir::ListKind::And, es) => {
        let mut it = es.into_iter();
        let v = self.as_temp(it.next().expect("AND must have an argument"))?;
        let vs = it.map(|e| self.operand(e)).collect::<Block<_>>()?;
        RValue::Pun(PunKind::And(vs), v.into())
      }
      hir::ExprKind::Ghost(e) => RValue::Ghost(self.copy_or_move(*e)?),
      hir::ExprKind::Borrow(e) => RValue::Borrow(self.place(*e)?),
      hir::ExprKind::Mm0(types::Mm0Expr {expr, subst}) => RValue::Mm0(expr,
        subst.into_iter().map(|e| self.as_temp(e).map(Into::into))
          .collect::<Block<Box<[_]>>>()?),
      hir::ExprKind::Cast(e, _, hir::CastKind::Ptr) =>
        RValue::Pun(PunKind::Ptr, self.expr_place(*e)?),
      hir::ExprKind::Cast(e, _, ck) => {
        let e_ty = e.k.1.1;
        let e = self.operand(*e)?;
        let ck = match ck {
          hir::CastKind::Int => CastKind::Int,
          hir::CastKind::Ptr => unreachable!(),
          hir::CastKind::Shr => CastKind::Shr,
          hir::CastKind::Subtype(h) => CastKind::Subtype(self.operand(*h)?),
          hir::CastKind::Wand(h) => CastKind::Wand(h.map(|h| self.operand(*h)).transpose()?),
          hir::CastKind::Mem(h) => CastKind::Mem(self.operand(*h)?),
        };
        RValue::Cast(ck, e, self.tr(e_ty))
      }
      hir::ExprKind::Uninit => Constant::uninit_core(self.tr(e.k.1.1)).into(),
      hir::ExprKind::Sizeof(ty) => Constant::sizeof(Size::Inf, self.tr(ty)).into(),
      hir::ExprKind::Typeof(e) => RValue::Typeof(self.operand(*e)?),
      hir::ExprKind::Assert { cond, trivial: None } => {
        let span = e.span.clone();
        if let Some(pe) = e.k.1.0 {
          let e = self.operand(*cond)?;
          let pe = self.tr(pe);
          self.assert(span, e, pe).into()
        } else {
          let v = self.as_temp(*cond)?;
          self.assert(span, Operand::Move(v.into()), Rc::new(ExprKind::Var(v))).into()
        }
      }
      hir::ExprKind::Assign {..} => {
        self.expr(e, None)?;
        Constant::unit().into()
      }
      hir::ExprKind::Call(ref call)
      if matches!(call.rk, hir::ReturnKind::Struct(_)) => {
        let hir::ExprKind::Call(call) = e.k.0 else { unreachable!() };
        let hir::ReturnKind::Struct(n) = call.rk else { unreachable!() };
        let dest = (0..n).map(|_| {
          hir::Spanned { span: e.span, k: PreVar::Ok(self.fresh_var()) }
        }).collect::<Vec<_>>();
        self.expr_call(e.span, call, e.k.1.1, &dest)?;
        RValue::List(dest.into_iter().map(|v| {
          let PreVar::Ok(v) = v.k else { unreachable!() };
          v.into()
        }).collect())
      }
      hir::ExprKind::Mm0Proof(p) => Constant::mm0_proof(self.tr(e.k.1.1), p).into(),
      hir::ExprKind::Block(bl) => self.rvalue_block(e.span, bl, Some(e.k.1))?,
      hir::ExprKind::While(while_) => self.rvalue_while(e.span, *while_)?,
      hir::ExprKind::Assert { trivial: Some(false), .. } |
      hir::ExprKind::Unreachable(_) |
      hir::ExprKind::Jump(_, _, _, _) |
      hir::ExprKind::Break(_, _) |
      hir::ExprKind::Return(_) |
      hir::ExprKind::UnpackReturn(_) => {
        self.expr(e, None)?;
        unreachable!()
      }
      hir::ExprKind::Var(_, _) |
      hir::ExprKind::Index(_) |
      hir::ExprKind::Slice(_) |
      hir::ExprKind::Proj(_, _, _) |
      hir::ExprKind::Deref(_) |
      hir::ExprKind::Rval(_) |
      hir::ExprKind::Ref(_) |
      hir::ExprKind::ArgRef(_) |
      hir::ExprKind::Unit |
      hir::ExprKind::ITrue |
      hir::ExprKind::Bool(_) |
      hir::ExprKind::Int(_) |
      hir::ExprKind::Const(_) |
      hir::ExprKind::Call(_) |
      hir::ExprKind::Assert { trivial: Some(true), .. } |
      hir::ExprKind::If {..} => self.operand(e)?.into(),
      hir::ExprKind::Error => unreachable!(),
    })
  }

  fn as_unit_const(&mut self, ty: ty::Ty<'a>) -> Constant {
    match ty.k {
      ty::TyKind::Unit => Constant::unit(),
      ty::TyKind::Pure(&ty::WithMeta {k: ty::ExprKind::Bool(true), ..}) |
      ty::TyKind::True => Constant::itrue(),
      ty::TyKind::Uninit(ty) => Constant::uninit(self.tr(ty)),
      _ => panic!("call is_unit_dest first"),
    }
  }

  fn fulfill_unit_dest<R>(&mut self, ety: ty::ExprTy<'a>,
    dest: Dest<'a>, f: impl FnOnce(&mut Self, Dest<'a>) -> Block<R>
  ) -> Block<R> {
    if let Some(v) = dest {
      if !ety.1.is_unit_dest() { return f(self, dest) }
      let r = f(self, None)?;
      let rv = self.as_unit_const(ety.1);
      let v = self.tr(v).cloned();
      let rel = !ety.1.ghostly();
      let (e, ty) = self.tr(ety);
      self.push_stmt(Statement::Let(LetKind::Let(v, e), rel, ty, rv.into()));
      Ok(r)
    } else { f(self, dest) }
  }

  fn expr(&mut self, e: hir::Expr<'a>, dest: Dest<'a>) -> Block<()> {
    self.fulfill_unit_dest(e.k.1, dest, |this, dest| {
      match e.k.0 {
        hir::ExprKind::If { hyp, cond, cases, gen_, muts, trivial } =>
          return this.expr_if(e.k.1, hyp, *cond, *cases, gen_, muts, trivial, dest),
        hir::ExprKind::Call(ref call) if matches!(call.rk, hir::ReturnKind::One) => {
          let hir::ExprKind::Call(call) = e.k.0 else { unreachable!() };
          return this.expr_call(e.span, call, e.k.1.1,
            &[dest.unwrap_or(hir::Spanned { span: e.span, k: PreVar::Fresh })])
        }
        _ => {}
      }
      match dest {
        None => match e.k.0 {
          hir::ExprKind::Unit |
          hir::ExprKind::ITrue |
          hir::ExprKind::Var(_, _) |
          hir::ExprKind::Const(_) |
          hir::ExprKind::Bool(_) |
          hir::ExprKind::Int(_) |
          hir::ExprKind::Uninit |
          hir::ExprKind::Sizeof(_) |
          hir::ExprKind::Assert { trivial: Some(true), .. } => {}
          hir::ExprKind::Unop(_, e) |
          hir::ExprKind::Rval(e) |
          hir::ExprKind::Ghost(e) |
          hir::ExprKind::Cast(e, _, _) |
          hir::ExprKind::Typeof(e) => return this.expr(*e, None),
          hir::ExprKind::Deref(e) |
          hir::ExprKind::Proj(_, e, _) => return this.expr(e.1, None),
          hir::ExprKind::Ref(e) |
          hir::ExprKind::ArgRef(e) |
          hir::ExprKind::Borrow(e) => return this.ignore_place(*e),
          hir::ExprKind::Binop(_, e1, e2) |
          hir::ExprKind::Eq(_, _, e1, e2) => {
            this.expr(*e1, None)?;
            this.expr(*e2, None)?;
          }
          hir::ExprKind::Sn(e1, h) => {
            this.expr(*e1, None)?;
            if let Some(h) = h { this.expr(*h, None)?; }
          }
          hir::ExprKind::Index(args) => {
            let (_, [arr, idx], hyp_or_n) = *args;
            this.expr(arr, None)?;
            this.expr(idx, None)?;
            if let Ok(hyp) = hyp_or_n { this.expr(hyp, None)?; }
          }
          hir::ExprKind::Slice(args) => {
            let (_, [arr, idx, len], hyp_or_n) = *args;
            this.expr(arr, None)?;
            this.expr(idx, None)?;
            this.expr(len, None)?;
            if let Ok(hyp) = hyp_or_n { this.expr(hyp, None)?; }
          }
          hir::ExprKind::List(_, es) => for e in es { this.expr(e, None)? }
          hir::ExprKind::Mm0(e) => for e in e.subst { this.expr(e, None)? }
          hir::ExprKind::Assert { trivial: None, .. } => {
            let span = dest.map_or(e.span, |v| v.span);
            this.expr(e, Some(hir::Spanned { span, k: PreVar::Fresh }))?
          }
          hir::ExprKind::Assign {lhs, rhs, map, gen_} => {
            let ty = lhs.ty();
            let lhs = this.place(*lhs)?;
            let rhs = this.operand(*rhs)?;
            let ty = this.tr(ty);
            let varmap = map.iter()
              .map(|(new, old, _)| (old.k, this.tr(new.k))).collect::<HashMap<_, _>>();
            this.tr.add_gen(this.tr.cur_gen, gen_, varmap);
            let vars = map.into_vec().into_iter().map(|(new, _, ety)| Rename {
              from: this.tr(new.k),
              to: Spanned { span: new.span.clone(), k: this.tr_gen(new.k, gen_) },
              rel: true,
              ety: this.tr_gen(ety, gen_)
            }).collect::<Box<[_]>>();
            this.tr.cur_gen = gen_;
            this.push_stmt(Statement::Assign(lhs, ty, rhs, vars))
          }
          hir::ExprKind::Mm0Proof(_) |
          hir::ExprKind::Block(_) |
          hir::ExprKind::While {..} => { this.rvalue(e)?; }
          hir::ExprKind::Call(call) => match call.rk {
            hir::ReturnKind::Unreachable |
            hir::ReturnKind::Unit => this.expr_call(e.span, call, e.k.1.1, &[])?,
            hir::ReturnKind::One => unreachable!(),
            hir::ReturnKind::Struct(n) =>
              this.expr_call(e.span, call, e.k.1.1,
                &vec![hir::Spanned { span: e.span, k: PreVar::Fresh }; n.into()])?,
          }
          hir::ExprKind::Assert { trivial: Some(false), .. } => {
            this.cur_block().terminate(Terminator::Fail);
            return Err(Diverged)
          }
          hir::ExprKind::Unreachable(h) => {
            let h = this.as_temp(*h)?;
            this.cur_block().terminate(Terminator::Unreachable(h.into()));
            return Err(Diverged)
          }
          hir::ExprKind::Jump(lab, i, es, variant) => {
            let (jp, jumps) = this.labels.iter()
              .rfind(|p| p.0 == lab).expect("missing label")
              .1.jumps.as_ref().expect("label does not expect jump");
            let (tgt, args) = jumps[usize::from(i)].clone();
            let jb = JoinBlock(tgt, jp.clone());
            let args = args.iter().zip(es).map(|(&(v, r), e)| {
              Ok((v, r, this.operand(e)?))
            }).collect::<Block<Vec<_>>>()?;
            let variant = variant.map(|v| this.operand(*v)).transpose()?;
            this.join(&jb, args, variant);
            return Err(Diverged)
          }
          hir::ExprKind::Break(lab, e) => {
            let (jb, dest) = this.labels.iter()
              .rfind(|p| p.0 == lab).expect("missing label")
              .1.brk.as_ref().expect("label does not expect break").clone();
            let args = match dest {
              None => { this.expr(*e, None)?; vec![] }
              Some((v, _)) => vec![(v.k, !e.k.1.1.ghostly(), this.operand(*e)?)]
            };
            this.join(&jb, args, None);
            return Err(Diverged)
          }
          hir::ExprKind::Return(es) =>
            match this.expr_return(|_| es.into_iter(), Self::expr_place)? {}
          hir::ExprKind::UnpackReturn(e) => {
            let pl = this.expr_place(e.1)?;
            let ty = this.tr(e.0);
            match this.expr_return(|n| 0..n.try_into().expect("overflow"), |_, i| Ok({
              let mut pl = pl.clone();
              pl.proj.push((ty.clone(), Projection::Proj(ListKind::Struct, i)));
              pl
            }))? {}
          }
          hir::ExprKind::If {..} | hir::ExprKind::Error => unreachable!()
        }
        Some(dest) => {
          let ety = e.k.1;
          let rv = this.rvalue(e)?;
          let dest = this.tr(dest).cloned();
          let rel = !ety.1.ghostly();
          let (e, ty) = this.tr(ety);
          this.push_stmt(Statement::Let(LetKind::Let(dest, e), rel, ty, rv))
        }
      }
      Ok(())
    })
  }

  fn tup_pat(&mut self, span: &'a FileSpan,
    global: bool, pat: TuplePattern<'a>, e_src: EPlace, src: &mut Place
  ) {
    match pat.k.k {
      TuplePatternKind::Name(name) => {
        let v = self.tr(pat.k.var);
        let src = if global {
          let tgt = self.tr(pat.k.ty);
          let r = !pat.k.ty.ghostly();
          let lk = LetKind::Let(Spanned { span: span.clone(), k: v }, None);
          self.push_stmt(Statement::Let(lk, r, tgt.clone(), src.clone().into()));
          self.globals.push((name, r, v, tgt));
          (Rc::new(EPlaceKind::Var(v)), v.into())
        } else {
          (e_src, src.clone())
        };
        self.tr.vars.insert(v, src);
      }
      TuplePatternKind::Tuple(pats, mk) => {
        let pk = match mk {
          TupleMatchKind::Unit | TupleMatchKind::True => return,
          TupleMatchKind::List | TupleMatchKind::Struct => ListKind::Struct,
          TupleMatchKind::Array => ListKind::Array,
          TupleMatchKind::And => ListKind::And,
          TupleMatchKind::Sn => ListKind::Sn,
          TupleMatchKind::Own |
          TupleMatchKind::Shr => {
            let [v_pat, h_pat] = *pats else { unreachable!() };
            let tgt = self.tr(pat.k.ty);
            let v = self.tr(v_pat.k.var);
            let h = self.tr(h_pat.k.var);
            let lk = LetKind::Ptr([
              (Spanned { span: span.clone(), k: v }, self.tr(v_pat.k.ty)),
              (Spanned { span: span.clone(), k: h }, self.tr(h_pat.k.ty))
            ]);
            self.push_stmt(Statement::Let(lk, true, tgt, src.clone().into()));
            self.tup_pat(span, global, v_pat, Rc::new(EPlaceKind::Var(v)), &mut v.into());
            self.tup_pat(span, global, h_pat, Rc::new(EPlaceKind::Var(h)), &mut h.into());
            return
          }
        };
        for (i, &pat) in pats.iter().enumerate() {
          let i = i.try_into().expect("overflow");
          let ty = self.tr(pat.k.ty);
          src.proj.push((ty.clone(), Projection::Proj(pk, i)));
          let e_src = Rc::new(EPlaceKind::Proj(e_src.clone(), ty, i));
          self.tup_pat(span, global, pat, e_src, src);
          src.proj.pop();
        }
      }
      TuplePatternKind::Error(_) => unreachable!()
    }
  }

  fn push_args_raw(&mut self,
    args: &[hir::Arg<'a>], mut f: impl FnMut(ty::ArgAttr, VarId, &Ty)
  ) -> Vec<(VarId, bool)> {
    let mut vs = Vec::with_capacity(args.len());
    for arg in args {
      if let hir::ArgKind::Lam(pat) = arg.1 {
        let var = self.tr(pat.k.k.var);
        vs.push((var, !pat.k.k.ty.ghostly()));
        let ty = self.tr(pat.k.k.ty);
        f(arg.0, var, &ty);
        let var = Spanned { span: pat.span.clone(), k: var };
        self.extend_ctx(var, !arg.0.contains(ty::ArgAttr::GHOST), (None, ty));
      }
    }
    vs
  }

  fn push_args(&mut self,
    args: Box<[hir::Arg<'a>]>, f: impl FnMut(ty::ArgAttr, VarId, &Ty)
  ) -> (BlockId, Rc<[(VarId, bool)]>) {
    let parent = self.cfg.ctxs.len(self.cur_ctx);
    let vs = self.push_args_raw(&args, f);
    let bl = self.new_block(parent);
    self.cur_block = bl;
    let mut pats = vec![];
    let mut it = vs.iter();
    for arg in args.into_vec() {
      match arg.1 {
        hir::ArgKind::Lam(pat) => {
          // Safety: In push_args_raw we push exactly one element for every Lam(..) in args
          let v = unsafe { it.next().unwrap_unchecked().0 };
          self.tr.tr_tup_pat(pat.k, Rc::new(ExprKind::Var(v)));
          self.tup_pat(pat.span, false, pat.k, Rc::new(EPlaceKind::Var(v)), &mut v.into());
          pats.push(pat.k);
        }
        hir::ArgKind::Let(pat, pe, e) => {
          if let Some(e) = e {
            let v = self.tr(pat.k.k.var);
            self.expr(*e, Some(pat.map_into(|_| PreVar::Ok(v))))
              .expect("pure expressions can't diverge");
            self.tup_pat(pat.span, false, pat.k, Rc::new(EPlaceKind::Var(v)), &mut v.into());
          }
          let pe = self.tr(pe);
          self.tr.tr_tup_pat(pat.k, pe);
          pats.push(pat.k);
        }
      }
    }
    for pat in pats.into_iter().rev() { self.tr.finish_tup_pat(pat) }
    (bl, vs.into())
  }

  fn join_args(&mut self, ctx: CtxId, &(gen_, ref muts): &JoinPoint,
    args: &mut Vec<(VarId, bool, Operand)>
  ) {
    args.extend(muts.iter().filter_map(|&v| {
      let from = self.tr(v);
      let to = self.tr_gen(v, gen_);
      if from == to {return None}
      let r = self.cfg.ctxs.rev_iter(ctx).find(|(u, _, _)| from == u.k)?.1;
      Some((to, r, from.into()))
    }));
  }

  fn join(&mut self,
    &JoinBlock(tgt, ref jp): &JoinBlock,
    mut args: Vec<(VarId, bool, Operand)>,
    variant: Option<Operand>,
  ) {
    let ctx = self.cfg[tgt].ctx;
    self.join_args(ctx, jp, &mut args);
    self.cur_block().terminate(Terminator::Jump(tgt, args.into(), variant))
  }

  fn let_stmt(&mut self, global: bool,
    lhs: hir::Spanned<'a, ty::TuplePattern<'a>>, rhs: hir::Expr<'a>
  ) -> Block<()> {
    if_chain! {
      if let hir::ExprKind::Call(hir::Call {rk: hir::ReturnKind::Struct(n), ..}) = rhs.k.0;
      if let ty::TuplePatternKind::Tuple(pats, _) = lhs.k.k.k;
      if pats.len() == usize::from(n);
      then {
        let hir::ExprKind::Call(call) = rhs.k.0 else { unreachable!() };
        let dest = pats.iter()
          .map(|&pat| lhs.map_into(|_| PreVar::Pre(pat.k.var)))
          .collect::<Vec<_>>();
        self.expr_call(rhs.span, call, rhs.k.1.1, &dest)?;
        for (&pat, v) in pats.iter().zip(dest) {
          let v = self.tr(v.k);
          self.tup_pat(lhs.span, global, pat, Rc::new(EPlaceKind::Var(v)), &mut v.into());
        }
        return Ok(())
      }
    }
    let v = PreVar::Pre(lhs.k.k.var);
    self.expr(rhs, Some(lhs.map_into(|_| v)))?;
    let v = self.tr(v);
    self.tup_pat(lhs.span, global, lhs.k, Rc::new(EPlaceKind::Var(v)), &mut v.into());
    Ok(())
  }

  fn stmt(&mut self, stmt: hir::Stmt<'a>, brk: Option<&(JoinBlock, BlockDest<'a>)>) -> Block<()> {
    match stmt.k {
      hir::StmtKind::Let { lhs, rhs } => self.let_stmt(false, lhs, rhs),
      hir::StmtKind::Expr(e) => self.expr(hir::Spanned {span: stmt.span, k: e}, None),
      hir::StmtKind::Label(v, has_jump, labs) => {
        let (brk, dest) = brk.expect("we need a join point for the break here");
        if has_jump {
          let base@(base_bl, base_ctx, base_gen) = self.cur();
          let mut bodies = vec![];
          let jumps = labs.into_vec().into_iter().map(|lab| {
            let (bl, args) = self.push_args(lab.args, |_, _, _| {});
            bodies.push(lab.body);
            self.cur_ctx = base_ctx;
            (bl, args)
          }).collect::<Rc<[_]>>();
          let bls: SmallVec<[_; 2]> = jumps.iter().map(|p| p.0).collect();
          self.cfg[base_bl].stmts.push(Statement::LabelGroup(bls.clone(), base_ctx));
          self.tree.push_group(bls);
          self.labels.push((v, LabelGroupData {
            jumps: Some(((base_gen, brk.1.1.clone()), jumps.clone())),
            brk: Some((brk.clone(), *dest))
          }));
          for (&(bl, _), body) in jumps.iter().zip(bodies) {
            self.set((bl, self.cfg[bl].ctx, base_gen));
            self.tree.push(bl);
            let dest2 = dest.as_ref().map(|v| v.0.map_into(PreVar::Ok));
            let ety = dest.as_ref().map(|v| v.1);
            if self.block(body.span, body.k, ety, dest2).is_ok() {
              let args = match dest {
                None => vec![],
                Some((v, _)) => vec![(v.k, true, v.k.into())]
              };
              self.join(brk, args, None)
            }
          }
          self.set(base);
        } else {
          self.labels.push((v, LabelGroupData {
            jumps: None, brk: Some((brk.clone(), *dest))
          }));
        }
        Ok(())
      }
    }
  }

  fn block(&mut self,
    span: &'a FileSpan, bl: hir::Block<'a>,
    ety: Option<ty::ExprTy<'a>>, dest: Dest<'a>
  ) -> Block<()> {
    let rv = self.rvalue_block(span, bl, ety)?;
    if let (Some(ety), Some(dest)) = (ety, dest) {
      let dest = self.tr(dest).cloned();
      let rel = !ety.1.ghostly();
      let (e, ty) = self.tr(ety);
      self.push_stmt(Statement::Let(LetKind::Let(dest, e), rel, ty, rv))
    }
    Ok(())
  }

  fn rvalue_block(&mut self,
    span: &'a FileSpan,
    hir::Block {stmts, expr, gen_, muts}: hir::Block<'a>,
    ret_ety: Option<ty::ExprTy<'a>>,
  ) -> Block<RValue> {
    let reset = (self.labels.len(), self.tree.groups.len());
    self.tr.try_add_gen(self.tr.cur_gen, gen_);
    let base_ctx = self.cur_ctx;
    let mut after_ctx = base_ctx;
    let jb = if stmts.iter().any(|s| matches!(s.k, hir::StmtKind::Label(..))) {
      let dest = ret_ety.map(|ety| {
        let v = self.fresh_var();
        let rel = !ety.1.ghostly();
        let ety2 = self.tr_gen(ety, gen_);
        self.extend_ctx(Spanned { span: span.clone(), k: v }, rel, ety2);
        (hir::Spanned { span, k: v }, ety)
      });
      let join = JoinBlock(self.dominated_block(base_ctx), (gen_, muts.into()));
      after_ctx = self.cur_ctx;
      self.cur_ctx = base_ctx;
      Some((join, dest))
    } else { None };
    let r = (|| {
      for stmt in stmts { self.stmt(stmt, jb.as_ref())? }
      let rv = if jb.is_some() {
        Err(if let Some(e) = expr { self.operand(*e)? } else { Constant::unit().into() })
      } else {
        Ok(if let Some(e) = expr { self.rvalue(*e)? } else { Constant::unit().into() })
      };
      let stmts = &mut self.cfg[self.cur_block].stmts;
      for _ in self.labels.len()..reset.0 { stmts.push(Statement::PopLabelGroup) }
      Ok(rv)
    })();
    self.labels.truncate(reset.0);
    self.tree.truncate(reset.1);
    if let Some((join, ref dest)) = jb {
      self.tree.push(join.0);
      if let Ok(rv) = r {
        let args = match dest {
          None => vec![],
          Some((v, _)) => vec![(v.k, true, rv.expect_err("impossible"))]
        };
        self.join(&join, args, None);
      }
      self.set((join.0, after_ctx, gen_));
      Ok(match dest { None => Constant::unit().into(), Some((v, _)) => v.k.into() })
    } else {
      r.map(|v| v.expect("impossible"))
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn expr_if(&mut self,
    ety: ty::ExprTy<'a>,
    hyp: Option<[hir::Spanned<'a, HVarId>; 2]>,
    cond: hir::Expr<'a>,
    [e_tru, e_fal]: [hir::Expr<'a>; 2],
    gen_: GenId,
    muts: Vec<HVarId>,
    trivial: Option<bool>,
    dest: Dest<'a>,
  ) -> Block<()> {
    // In the general case, we generate:
    //   v_cond := cond
    //   dominated_block(after)
    //   if v_cond {h. goto tru(h)} else {h. goto fal(h)}
    // tru(h: cond):
    //   v := e_tru
    //   goto after(v)
    // fal(h: !cond):
    //   v := e_fal
    //   goto after(v)
    // after(dest: T):
    let pe = cond.k.1.0;
    let cond_span = cond.span;
    //   v_cond := cond
    let v_cond = self.as_temp(cond)?;
    let pe = pe.map_or_else(|| Rc::new(ExprKind::Var(v_cond)), |e| self.tr(e));

    if let Some(b) = trivial {
      // If the condition is trivial, then we just go straight to the appropriate side of the if
      if let Some(hyp) = hyp {
        let vh_s = self.tr(if b { hyp[0] } else { hyp[1] }).cloned();
        self.push_stmt(Statement::Let(
          LetKind::Let(vh_s, Some(Rc::new(ExprKind::Unit))), false,
          Rc::new(TyKind::Pure(pe)),
          Constant::itrue().into()));
      }
      return self.expr(if b { e_tru } else { e_fal }, dest)
    }

    let (vh1_s, vh2_s) = match hyp {
      None => (self.fresh_var_span(cond_span.clone()), self.fresh_var_span(cond_span.clone())),
      Some([vh1, vh2]) => (self.tr(vh1).cloned(), self.tr(vh2).cloned()),
    };
    let (vh1, vh2) = (vh1_s.k, vh2_s.k);
    let base@(_, base_ctx, base_gen) = self.cur();
    self.tr.try_add_gen(base_gen, gen_);
    let base_len = self.cfg.ctxs.len(base_ctx);
    // tru_ctx is the current context with `vh: cond`
    let tru_ctx = self.cfg.ctxs.extend(base_ctx, vh1_s, false,
      (Some(Rc::new(ExprKind::Unit)), Rc::new(TyKind::Pure(pe.clone()))));
    let tru = self.cfg.new_block(tru_ctx, base_len);
    // fal_ctx is the current context with `vh: !cond`
    let fal_ctx = self.cfg.ctxs.extend(base_ctx, vh2_s, false,
      (Some(Rc::new(ExprKind::Unit)), Rc::new(TyKind::Not(Rc::new(TyKind::Pure(pe))))));
    let fal = self.cfg.new_block(fal_ctx, base_len);
    //   if v_cond {vh. goto tru(vh)} else {vh. goto fal(vh)}
    self.cur_block().terminate(Terminator::If(base_ctx, v_cond.into(), [(vh1, tru), (vh2, fal)]));

    let (trans, dest) = match dest {
      None => (None, None),
      Some(v) => {
        let x = self.tr_gen(v.k, gen_);
        (Some(v.map_into(|_| x)), Some(v.map_into(|_| PreVar::Ok(x))))
      }
    };
    // tru(h: cond):
    //   dest := e_tru
    self.set((tru, tru_ctx, base_gen));
    let tru_res = self.expr(e_tru, dest);
    let tru = self.cur();
    // fal(h: !cond):
    //   dest := e_fal
    self.set((fal, fal_ctx, base_gen));
    let fal_res = self.expr(e_fal, dest);
    let fal = self.cur();

    // Either `e_tru` or `e_fal` may have diverged before getting to the end of the block.
    // * If they both diverged, then the if statement diverges
    // * If one of them diverges, then the if statement degenerates:
    //     v_cond := cond
    //     if v_cond {h. goto tru(h)} else {h. goto fal(h)}
    //   tru(h: cond):
    //     e_tru -> diverge
    //   fal(h: !cond):
    //     dest := e_fal
    match (tru_res, fal_res) {
      (Err(Diverged), Err(Diverged)) => return Err(Diverged),
      (Ok(()), Err(Diverged)) => { self.set(tru) }
      (Err(Diverged), Ok(())) => { self.set(fal) }
      (Ok(()), Ok(())) => {
        // If neither diverges, we put `goto after` at the end of each block
        self.set(base);
        if let Some(v) = trans {
          let ety = self.tr_gen(ety, gen_);
          self.extend_ctx(v.cloned(), true, ety)
        }
        let after_ctx = self.cur_ctx;
        let join = JoinBlock(self.dominated_block(base_ctx), (gen_, muts.into()));
        self.set(tru);
        let args = match trans { None => vec![], Some(v) => vec![(v.k, true, v.k.into())] };
        self.join(&join, args.clone(), None);
        self.set(fal);
        self.join(&join, args, None);
        // And the after block is the end of the statement
        self.tree.push(join.0);
        self.set((join.0, after_ctx, gen_));
      }
    }
    Ok(())
  }

  #[allow(clippy::manual_assert)]
  fn rvalue_while(&mut self,
    span: &'a FileSpan,
    hir::While {
      label, has_break, hyp, cond,
      variant: _, body, gen_, muts, trivial
    }: hir::While<'a>,
  ) -> Block<RValue> {
    // If `has_break` is true, then this looks like
    //   dest: () := (while cond body)
    // otherwise
    //   dest: !cond := (while cond body).
    // We don't actually have `dest` here, we're making an rvalue that could potentially be placed
    // in a destination.

    // If `cond` evaluates to `false`, then we generate the code
    // _ := cond; dest: !cond := itrue
    // since we know that `!cond` is defeq to true
    if trivial == Some(false) {
      // ~>
      self.expr(*cond, None)?;
      return Ok(if has_break {Constant::unit()} else {Constant::itrue()}.into())
    }

    // Otherwise we actually have a loop. Generally we want to produce:
    //
    //   label_group([base])
    //   jump base
    // base:
    //   v := cond
    //   if v {h. goto main(h)} else {h'. goto after(h')}
    // main(h: cond):
    //   _ := body
    //   goto base
    // after(h': !cond):
    //   pop_label_group
    //   dest := h'
    //
    // If `has_break` is true, then we use an extra block `fal` to drop `!cond`:
    //
    //   dominated_block(after)
    //   label_group([base])
    //   jump base
    // base:
    //   v := cond
    //   if v {h. goto main(h)} else {h'. goto fal(h')}
    // fal(h': !cond):
    //   goto after
    // main(h: cond):
    //   _ := body
    //   goto base
    // after:
    //   dest := ()
    let base_ctx = self.cur_ctx;
    let base_ctx_len = self.cfg.ctxs.len(base_ctx);
    let base_bl = self.new_block(base_ctx_len);
    let muts: Rc<[HVarId]> = muts.into();
    // if `has_break` then we need to be prepared to jump to `after` from inside the loop.
    let brk = if has_break {
      Some((JoinBlock(self.dominated_block(base_ctx), (gen_, muts.clone())), None))
    } else { None };

    self.cur_block().stmts.push(
      Statement::LabelGroup(std::iter::once(base_bl).collect(), base_ctx));
    self.tree.push_group(std::iter::once(base_bl).collect());
    self.tree.push(base_bl);
    self.cur_block().terminate(Terminator::Jump(base_bl, Box::new([]), None));
    self.cur_block = base_bl;
    let base_gen = self.tr.cur_gen;
    self.tr.try_add_gen(base_gen, gen_);

    // Set things up so that `(continue label)` jumps to `base`,
    // and `(break label)` jumps to `after`
    self.labels.push((label, LabelGroupData {
      jumps: Some(((base_gen, muts.clone()), Rc::new([(base_bl, Rc::new([]))]))),
      brk: brk.clone()
    }));

    // `exit_point` captures the exit condition produced from inside the loop.
    let mut exit_point = Err(Diverged);
    // This is the body of the loop. We catch divergence here because if the execution of `cond` or
    // `body` diverges we still have to construct `after` and anything after that, provided that
    // `after` itself is reachable (which we signal by `exit_point` having a value).
    (|| -> Block<()> {
      let pe: Option<ty::Expr<'_>> = cond.k.1.0;
      if trivial == Some(true) {
        // If `cond` evaluates to `true`, then this is an infinite loop and `after` is not
        // reachable, unless it is accessed indirectly via `break`.
        // We generate the following code:
        //   label_group([base])
        //   jump base
        // base:
        //   _ := cond   (we know it is true so no point in capturing the result)
        //   h: cond := itrue    (force cond = true by defeq)
        //   _ := body
        //   goto base
        //
        // We never set `exit_point`, so it remains `Err(Diverged)` because it is unreachable.
        self.expr(*cond, None)?;
        if let (Some(pe), Some(hyp)) = (pe, hyp) {
          let pe = self.tr(pe);
          let vh = self.tr(hyp);
          self.push_stmt(Statement::Let(
            LetKind::Let(vh.cloned(), Some(Rc::new(ExprKind::Unit))), false,
            Rc::new(TyKind::Pure(pe)),
            Constant::itrue().into()));
        }
      } else {
        // Otherwise, this is a proper while loop.
        //   v_cond := cond
        let cond_span = cond.span;
        let v_cond = self.as_temp(*cond)?;
        let pe = pe.map_or_else(|| Rc::new(ExprKind::Var(v_cond)), |e| self.tr(e));
        let vh = match hyp {
          None => self.fresh_var_span(cond_span.clone()),
          Some(hyp) => self.tr(hyp).cloned()
        };
        let test = self.cur();
        let cur_len = self.cfg.ctxs.len(test.1);
        // tru_ctx is the current context with `vh: cond`
        let tru_ctx = self.cfg.ctxs.extend(test.1, vh.clone(), false,
          (Some(Rc::new(ExprKind::Unit)), Rc::new(TyKind::Pure(pe.clone()))));
        let tru = self.cfg.new_block(tru_ctx, cur_len);
        // fal_ctx is the current context with `vh: !cond`
        let fal_ctx = self.cfg.ctxs.extend(test.1, vh.clone(), false,
          (Some(Rc::new(ExprKind::Unit)), Rc::new(TyKind::Not(Rc::new(TyKind::Pure(pe))))));
        let fal = self.cfg.new_block(fal_ctx, cur_len);
        //   if v_cond {vh. goto main(vh)} else {vh. goto after(vh)}
        self.cur_block().terminate(
          Terminator::If(test.1, v_cond.into(), [(vh.k, tru), (vh.k, fal)]));

        // If `brk` is set (to `after`) then that means `has_break` is true so we want to jump to
        // `after` and ignore the `vh: !cond` in the false case.
        if let Some((ref join, _)) = brk {
          // We don't set `exit_point` because it is ignored in this case
          self.set((fal, fal_ctx, test.2));
          self.join(join, vec![], None);
        } else {
          // Pop the label group for the while because we are still in scope
          self.cfg[fal].stmts.push(Statement::PopLabelGroup);
          // Set `exit_point` to the `after` block (the false branch of the if),
          // and `vh: !cond` is the output
          exit_point = Ok(((fal, fal_ctx, test.2), vh.k));
        }
        // Prepare to generate the `main` label
        self.set((tru, tru_ctx, test.2));
      }
      //   _ := body
      self.rvalue_block(span, *body, None)?;
      // If we are checking termination, then this is a failure condition because
      // we don't have any proof to go with the back-edge.
      if crate::proof::VERIFY_TERMINATION {
        panic!("Add an explicit (continue) in the loop to prove termination");
      }
      //   goto base
      self.join(&JoinBlock(base_bl, (base_gen, muts)), vec![], None);
      // We've diverged, there is nothing more to do in the loop
      Err(Diverged)
    })().expect_err("it's a loop");

    self.tree.pop();
    if let Some((JoinBlock(tgt, (gen_, _)), _)) = self.labels.pop().expect("underflow").1.brk {
      // If `has_break` is true, then the final location after the while loop is the join point
      // `after`, and the context is `base_ctx` because the while loop doesn't add any bindings
      self.tree.push(tgt);
      self.set((tgt, base_ctx, gen_));
      // We return `()` from the loop expression
      Ok(Constant::unit().into())
    } else {
      // If `has_break` is false, then the final location is the false branch inside the loop,
      // which we may or may not have reached before diverging (if for example the loop condition
      // diverges). If we did reach it then we pull out the position and return the value.
      exit_point.map(|(pos, v)| {
        self.tree.push(pos.0);
        self.set(pos);
        v.into()
      })
    }
  }

  fn expr_call(&mut self, span: &'a FileSpan,
    hir::Call {f, side_effect: se, tys, args, variant, gen_, rk}: hir::Call<'a>,
    tgt: ty::Ty<'a>,
    dest: &[hir::Spanned<'a, PreVar>],
  ) -> Block<()> {
    if variant.is_some() {
      unimplemented!("recursive functions not supported")
    }
    let tys = self.tr(tys);
    let args = args.into_iter().map(|e| Ok((!e.k.1.1.ghostly(), self.operand(e)?)))
      .collect::<Block<Box<[_]>>>()?;
    let base_ctx = self.cur_ctx;
    let base_len = self.cfg.ctxs.len(base_ctx);
    self.tr.try_add_gen(self.tr.cur_gen, gen_);
    self.tr.cur_gen = gen_;
    let vars: Box<[_]> = match rk {
      hir::ReturnKind::Unreachable => {
        let v_s = self.fresh_var_span(span.clone());
        let v = v_s.k;
        self.extend_ctx(v_s, false, (None, Rc::new(TyKind::False)));
        let bl = self.new_block(base_len);
        self.cur_block().terminate(Terminator::Call {
          ctx: base_ctx, f: f.k, se, tys, args, reach: false, tgt: bl, rets: Box::new([(false, v)])
        });
        let bl = &mut self.cfg[bl];
        bl.reachable = false;
        bl.terminate(Terminator::Unreachable(v.into()));
        return Err(Diverged)
      }
      hir::ReturnKind::Unit => Box::new([]),
      hir::ReturnKind::One => {
        let v = self.tr(dest[0]);
        let vr = !tgt.ghostly();
        let tgt = self.tr(tgt);
        self.extend_ctx(v.cloned(), vr, (None, tgt));
        Box::new([(vr, v.k)])
      }
      hir::ReturnKind::Struct(_) => {
        let tgt = self.tr(tgt);
        let argtys = if let TyKind::Struct(args) = &*tgt { &**args } else { unreachable!() };
        let mut alph = Alpha::default();
        dest.iter().zip(argtys).map(|(&dest, &Arg {attr, var, ref ty})| {
          let v = self.tr(dest);
          let ty = alph.alpha(ty);
          let vr = !attr.contains(ArgAttr::GHOST);
          self.extend_ctx(v.cloned(), vr, (None, ty));
          if !attr.contains(ArgAttr::NONDEP) { alph.push(var, v.k) }
          (vr, v.k)
        }).collect()
      }
    };
    let bl = self.new_block(base_len);
    self.cur_block().terminate(Terminator::Call {
      ctx: base_ctx, f: f.k, se, tys, args, reach: true, tgt: bl, rets: vars
    });
    self.cur_block = bl;
    Ok(())
  }

  fn expr_return<T, I: ExactSizeIterator<Item=T>>(&mut self,
    es: impl FnOnce(usize) -> I,
    mut f: impl FnMut(&mut Self, T) -> Block<Place>,
  ) -> Block<std::convert::Infallible> {
    let Returns { outs, args } = &*self.returns.as_ref().expect("can't return here").clone();
    let args = es(args.len()).zip(&**args).map(|(e, &(v, r))| {
      Ok((v, r, f(self, e)?.into()))
    }).collect::<Block<Box<[_]>>>()?;
    let outs = outs.iter().map(|&out| self.tr(out)).collect();
    self.cur_block().terminate(Terminator::Return(outs, args));
    Err(Diverged)
  }

  /// Build the MIR for an item (function, procedure, or static variable).
  pub(crate) fn build_item(mut self,
    mir: &mut HashMap<Symbol, Proc>,
    init: &mut Initializer,
    it: hir::Item<'a>
  ) -> Option<Symbol> {
    self.cfg.span = it.span.clone();
    match it.k {
      hir::ItemKind::Proc { kind, name, tyargs, args, gen_, outs, rets, variant, body } => {
        fn tr_attr(attr: ty::ArgAttr) -> ArgAttr {
          let mut out = ArgAttr::empty();
          if attr.contains(ty::ArgAttr::NONDEP) { out |= ArgAttr::NONDEP }
          if attr.contains(ty::ArgAttr::GHOST) { out |= ArgAttr::GHOST }
          out
        }
        if variant.is_some() {
          unimplemented!("recursive functions not supported")
        }
        let outs2 = outs.iter().map(|&i| args[u32_as_usize(i)].1.var().k.k.var)
          .collect::<Box<[_]>>();
        let mut args2 = Vec::with_capacity(args.len());
        assert_eq!(self.push_args(args, |attr, var, ty| {
          args2.push(Arg {attr: tr_attr(attr), var, ty: ty.clone()})
        }).0, BlockId::ENTRY);
        let base_ctx = self.cur_ctx;
        self.tr.try_add_gen(GenId::ROOT, gen_);
        self.tr.cur_gen = gen_;
        let mut rets2 = Vec::with_capacity(rets.len());
        let ret_vs = self.push_args_raw(&rets, |attr, var, ty| {
          rets2.push(Arg {attr: tr_attr(attr), var, ty: ty.clone()})
        })[outs2.len()..].into();
        self.returns = Some(Rc::new(Returns { outs: outs2, args: ret_vs }));
        self.tr.cur_gen = GenId::ROOT;
        self.cur_ctx = base_ctx;
        let Err(Diverged) = self.block(it.span, body, None, None) else {
          unreachable!("bodies should end in unconditional return")
        };
        self.cfg.max_var = self.tr.next_var;
        self.tree.append_to(&mut self.cfg.tree);
        mir.insert(name.k, Proc {
          kind,
          name: Spanned {span: name.span.clone(), k: name.k},
          tyargs,
          args: args2,
          outs,
          rets: rets2,
          body: self.cfg,
          allocs: None,
        });
        Some(name.k)
      }
      hir::ItemKind::Global { lhs, rhs } => {
        mem::swap(&mut init.cfg, &mut self.cfg);
        mem::swap(&mut init.globals, &mut self.globals);
        self.tr.next_var = self.cfg.max_var;
        init.cur = (|| {
          self.set(init.cur?);
          self.let_stmt(true, lhs, rhs)?;
          Ok(self.cur())
        })();
        self.cfg.max_var = self.tr.next_var;
        self.tree.append_to(&mut self.cfg.tree);
        init.cfg = self.cfg;
        init.globals = self.globals;
        None
      }
    }
  }
}

impl Initializer {
  /// Append the call to `main` at the end of the `start` routine.
  pub(crate) fn finish(&mut self,
    mir: &HashMap<Symbol, Proc>, main: Option<Symbol>
  ) -> (Cfg, Vec<(Symbol, bool, VarId, Ty)>) {
    let mut build = BuildMir::new(None);
    mem::swap(&mut self.cfg, &mut build.cfg);
    mem::swap(&mut self.globals, &mut build.globals);
    build.tr.next_var = build.cfg.max_var;
    let _ = (|| -> Block<()> {
      build.set(self.cur?);
      let o = if let Some(main) = main {
        let body = &mir[&main];
        let base_ctx = build.cur_ctx;
        let base_len = build.cfg.ctxs.len(base_ctx);
        let (rets, o): (Box<[_]>, _) = match *body.rets {
          [ref ret] => {
            let v_s = build.fresh_var_span(body.name.span.clone());
            let v = v_s.k;
            build.extend_ctx(v_s, false, (None, ret.ty.clone()));
            (Box::new([(false, v)]), v.into())
          }
          [] => (Box::new([]), Constant::unit().into()),
          _ => panic!("main should have at most one return")
        };
        let tgt = build.new_block(base_len);
        build.cur_block().terminate(Terminator::Call {
          ctx: base_ctx,
          f: main,
          tys: Box::new([]),
          se: true,
          args: Box::new([]), // TODO
          reach: true,
          tgt,
          rets,
        });
        build.cur_block = tgt;
        o
      } else {
        Constant::unit().into()
      };
      build.cur_block().terminate(Terminator::Exit(o));
      Ok(())
    })();
    build.tree.append_to(&mut build.cfg.tree);
    (build.cfg, build.globals)
  }
}
