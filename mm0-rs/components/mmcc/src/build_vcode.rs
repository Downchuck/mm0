//! The translation pass from [`MIR`](crate::types::mir) to [`VCode`](crate::types::vcode).
//!
//! While MIR is an abstract intermediate language with generic operations,
//! the [`VCode`] type is much closer to the hardware ISA, and most operations
//! in [`crate::arch::Inst`] have one to one correspondence to instructions of
//! the ISA, except that they use virtual registers instead of physical
//! registers. So the main role of this pass is to translate MIR operations
//! into sequences of x86 instructions.

use std::collections::HashMap;
use std::rc::Rc;

use arrayvec::ArrayVec;
use mm0_util::FileSpan;
use regalloc2::Operand as ROperand;

use crate::linker::ConstData;
use crate::types::entity::{IntrinsicProc, ProcTc, ProcTy};
use crate::{Symbol, Entity};
use crate::arch::{AMode, Binop as VBinop, CC, Cmp, ExtMode, Inst, PReg, RegMem, RegMemImm,
  RET_AND_ARG_REGS, SYSCALL_ARG_REGS, ShiftKind, SysCall, Unop as VUnop};
use crate::mir_opt::BitSet;
use crate::mir_opt::storage::{Allocations, AllocId};
use crate::types::{Idx, IdxVec, IntTy, Size, Spanned, classify as cl};
use crate::types::vcode::{self, ArgAbi, BlockId as VBlockId,
  ChunkVec, ConstRef, InstId, GlobalId, ProcAbi, ProcId, SpillId, VReg, VRegRename, IsReg};
use crate::arch::Offset;

#[allow(clippy::wildcard_imports)]
use crate::types::mir::*;

pub(crate) type VCode = vcode::VCode<Inst>;

/// A very simple jump threading visitor. Start at an unvisited basic block, then follow forward
/// edges to unvisited basic blocks as long as possible. Then start over somewhere else.
/// This ordering is good for code placement since a jump or branch to the immediately following
/// block can be elided.
fn visit_blocks<'a>(
  cfg: &'a Cfg,
  mut f: impl FnMut(BlockId, &'a BasicBlock) -> Result<(), LowerErr>
) -> Result<(), LowerErr> {
  if !cfg[BlockId::ENTRY].reachable {
    return Err(LowerErr::EntryUnreachable(cfg.span.clone()))
  }
  let mut visited: BitSet<BlockId> = BitSet::default();
  for (mut i, mut bl) in cfg.blocks() {
    if visited.insert(i) && bl.reachable {
      while let Some((_, j)) = {
        f(i, bl)?;
        bl.successors().find(|&(_, j)| visited.insert(j))
      } {
        i = j;
        bl = &cfg[i];
      }
    }
  }
  Ok(())
}

struct TyCtx<'a> {
  cfg: &'a Cfg,
  ctx: HashMap<VarId, (&'a FileSpan, Ty)>,
}

impl<'a> TyCtx<'a> {
  fn new(cfg: &'a Cfg) -> Self { Self { cfg, ctx: Default::default() } }

  fn insert(&mut self, v: VarId, sp: &'a FileSpan, ty: Ty) { self.ctx.insert(v, (sp, ty)); }

  fn start_block(&mut self, bl: &'a BasicBlock) {
    self.ctx.clear();
    for (v, _, (_, ty)) in bl.ctx_rev_iter(&self.cfg.ctxs) {
      self.insert(v.k, &v.span, ty.clone());
    }
  }
}

#[derive(Debug)]
enum VRetAbi {
  /// The value is not passed.
  Ghost,
  /// The value is passed in the given physical register.
  Reg(PReg, Size),
  /// The value is passed in a memory location.
  Mem {
    /// The offset in the `OUTGOING` slot to find the data.
    off: u32,
    /// The size of the data in bytes.
    sz: u32
  },
  /// A pointer to a value of the given size is passed in a physical register.
  /// Note: For return values with this ABI, this is an additional argument *to* the function:
  /// the caller passes a pointer to the return slot.
  Boxed {
    /// The register carrying the pointer.
    reg: (VReg, PReg),
    /// The size of the pointed-to data in bytes.
    sz: u32
  },
  /// A pointer to the value is passed in memory. This is like `Boxed`,
  /// but for the case that we have run out of physical registers.
  /// (The pointer is at `off..off+8`, and the actual value is at `[off]..[off]+sz`.)
  /// Note: For return values with this ABI, this is an additional argument *to* the function:
  /// the caller puts a pointer to the return slot at this location in the outgoing slot.
  BoxedMem {
    /// The offset in the `OUTGOING` slot to find the pointer. (It has a fixed size of 8.)
    off: u32,
    /// The size of the data starting at the pointer location.
    sz: u32
  },
}

impl From<&VRetAbi> for ArgAbi {
  fn from(abi: &VRetAbi) -> Self {
    match *abi {
      VRetAbi::Ghost => ArgAbi::Ghost,
      VRetAbi::Reg(reg, sz) => ArgAbi::Reg(reg, sz),
      VRetAbi::Mem { off, sz } => ArgAbi::Mem { off, sz },
      VRetAbi::Boxed { reg: (_, reg), sz } => ArgAbi::Boxed { reg, sz },
      VRetAbi::BoxedMem { off, sz } => ArgAbi::BoxedMem { off, sz },
    }
  }
}

enum GhostErr {
  GhostVarUsed(VarId),
  InfiniteOp,
}

/// Errors that can occur during lowering. Several user-level errors make it to this stage;
/// the main one is that a ghost variable is used in a computationally relevant position.
#[derive(Debug)]
pub enum LowerErr {
  /// A ghost variable was used in an operation that requires it to not be ghost.
  GhostVarUsed(Spanned<VarId>),
  /// An operation on unbounded integers was used and could not be optimized away.
  InfiniteOp(FileSpan),
  /// The entry point is unreachable, which means that there is an
  /// unconditional infinite loop in the function.
  EntryUnreachable(FileSpan),
}
/// The ABI expected by the caller.
#[derive(Clone, Copy, Debug)]
pub(crate) enum VCodeCtx<'a> {
  /// This is a regular procedure; the `&[Arg]` is the function returns.
  Proc(&'a [Arg]),
  /// This is the `start` function, which is called by the operating system and has a
  /// special stack layout.
  Start(&'a [(Symbol, bool, VarId, Ty)]),
}

impl<'a> From<&'a [Arg]> for VCodeCtx<'a> {
    fn from(v: &'a [Arg]) -> Self { Self::Proc(v) }
}

struct LowerCtx<'a> {
  cfg: &'a Cfg,
  allocs: &'a Allocations,
  names: &'a HashMap<Symbol, Entity>,
  func_mono: &'a HashMap<Symbol, ProcId>,
  funcs: &'a IdxVec<ProcId, ProcAbi>,
  consts: &'a ConstData,
  code: VCode,
  var_map: HashMap<AllocId, (RegMem, Size)>,
  ctx: TyCtx<'a>,
  unpatched: Vec<(VBlockId, InstId)>,
  globals: HashMap<AllocId, GlobalId>,
  abi_args: Vec<ArgAbi>,
  abi_rets: Rc<[VRetAbi]>,
  can_return: bool,
}

impl<'a> LowerCtx<'a> {
  /// Create a new lowering context.
  fn new(
    names: &'a HashMap<Symbol, Entity>,
    func_mono: &'a HashMap<Symbol, ProcId>,
    funcs: &'a IdxVec<ProcId, ProcAbi>,
    consts: &'a ConstData,
    cfg: &'a Cfg,
    allocs: &'a Allocations,
    ctx: VCodeCtx<'_>,
  ) -> Self {
    LowerCtx {
      cfg,
      allocs,
      names,
      func_mono,
      funcs,
      consts,
      code: VCode::default(),
      var_map: HashMap::new(),
      ctx: TyCtx::new(cfg),
      unpatched: vec![],
      abi_args: vec![],
      abi_rets: Rc::new([]),
      can_return: cfg.can_return(),
      globals: match ctx {
        VCodeCtx::Proc(_) => HashMap::new(),
        VCodeCtx::Start(ls) => {
          let mut map = HashMap::new();
          for (id, &(_, r, v, _)) in ls.iter().enumerate() {
            if r {
              let a = allocs.get(v);
              assert_ne!(a, AllocId::ZERO);
              assert!(map.insert(a, GlobalId::from_usize(id)).is_none(),
                "global allocation collision");
            }
          }
          map
        }
      }
    }
  }

  fn emit(&mut self, inst: Inst) -> InstId { self.code.emit(inst) }

  fn get_alloc(&mut self, a: AllocId) -> (&(RegMem, Size), u64) {
    assert_ne!(a, AllocId::ZERO);
    let m = self.allocs[a].m;
    (self.var_map.entry(a).or_insert_with(|| {
      let rm = if let Some(&id) = self.globals.get(&a) {
        RegMem::Mem(AMode::global(id))
      } else if m.on_stack {
        RegMem::Mem(AMode::spill(
          self.code.fresh_spill(m.size.try_into().expect("allocation too large"))))
      } else {
        RegMem::Reg(self.code.fresh_vreg())
      };
      (rm, Size::from_u64(m.size))
    }), m.size)
  }

  fn rename_alloc(&mut self, a: AllocId, r: VRegRename) {
    if let Some((RegMem::Reg(v), _)) = self.var_map.get_mut(&a) {
      *v = v.rename(r)
    }
  }

  fn get_var(&self, v: VarId) -> Result<&(RegMem, Size), GhostErr> {
    let a = self.allocs.get(v);
    if a == AllocId::ZERO { return Err(GhostErr::GhostVarUsed(v)) }
    Ok(&self.var_map[&a])
  }

  fn get_place(&mut self, p: &Place) -> Result<(RegMem, cl::Place), GhostErr> {
    let mut rm = self.get_var(p.local)?.0;
    let mut cl = cl::Place { projs: 0 };
    for proj in &p.proj {
      cl.projs += 1;
      let proj = match proj.1 {
        Projection::Deref => {
          let (a, cl) = rm.emit_deref(&mut self.code, Size::S64);
          rm = RegMem::Mem(a);
          cl::Projection::Deref(cl)
        }
        Projection::Proj(ListKind::And | ListKind::Sn, _) => cl::Projection::Ghost,
        Projection::Proj(ListKind::Array, i) => {
          let TyKind::Array(ty, _) = &*proj.0 else { unreachable!() };
          let sz = ty.sizeof(self.names)
            .expect("array element size not known at compile time");
          let off = u32::try_from(sz).expect("overflow") * i;
          if off != 0 {
            match &mut rm {
              RegMem::Reg(_) => panic!("register should be address-taken"),
              RegMem::Mem(a) => *a = &*a + off,
            }
          }
          cl::Projection::ProjArray
        }
        Projection::Proj(ListKind::Struct, i) => {
          let TyKind::Struct(args) = &*proj.0 else { unreachable!() };
          let off = args[..i as usize].iter().map(|arg| {
            if arg.attr.contains(ArgAttr::GHOST) { return 0 }
            let sz = arg.ty.sizeof(self.names)
              .expect("struct element size not known at compile time");
            u32::try_from(sz).expect("overflow")
          }).sum();
          if off != 0 {
            match &mut rm {
              RegMem::Reg(_) => panic!("register should be address-taken"),
              RegMem::Mem(a) => *a = &*a + off,
            }
          }
          cl::Projection::ProjStruct
        }
        Projection::Index(i, _) |
        Projection::Slice(i, _, _) => {
          let TyKind::Array(ty, _) = &*proj.0 else { unreachable!() };
          let Some(stride) = ty.sizeof(self.names) else {
            panic!("array stride not known at compile time")
          };
          if stride == 0 {
            cl::Projection::Ghost
          } else {
            let (v, sz) = *self.get_var(i)?;
            let (v, cl) = v.into_reg(&mut self.code, sz);
            match &mut rm {
              RegMem::Reg(_) => panic!("register should be address-taken"),
              RegMem::Mem(a) => {
                let (a2, cl2) = a.add_scaled(&mut self.code, stride, v);
                *a = a2;
                cl::Projection::IndexSlice(cl, cl2)
              }
            }
          }
        }
      };
      self.code.trace.projs.push(proj);
    }
    Ok((rm, cl))
  }

  fn get_const(&self, c: &Constant) -> (u32, ConstRef) {
    match c.k {
      ConstKind::Bool => {
        let Some(e) = &c.ety.0 else { unreachable!() };
        let ExprKind::Bool(b) = **e else { unreachable!() };
        (1, ConstRef::Value(b.into()))
      }
      ConstKind::Int => {
        let Some(e) = &c.ety.0 else { unreachable!() };
        let ExprKind::Int(n) = &**e else { unreachable!() };
        let TyKind::Int(ity) = *c.ety.1 else { unreachable!() };
        let n = ity.zero_extend_as_u64(n).expect("impossible");
        (ity.size().bytes().expect("constant too large to compile").into(), ConstRef::Value(n))
      }
      ConstKind::Uninit => {
        let sz = c.ety.1.sizeof(self.names).expect("size must be known at compile time");
        (sz.try_into().expect("overflow"), ConstRef::Value(0))
      }
      ConstKind::Const(c) => self.consts[c],
      ConstKind::Sizeof => {
        let (sz, ty) = c.ty_as_sizeof();
        let sizeof = ty.sizeof(self.names).expect("size must be known at compile time");
        (sz.bytes().expect("can't evaluate unbounded sizeof").into(), ConstRef::Value(sizeof))
      }
      ConstKind::Unit |
      ConstKind::ITrue |
      ConstKind::Mm0Proof(_) |
      ConstKind::Contra(_, _) => unreachable!("unexpected ZST"),
      #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
      ConstKind::As(ref c) => {
        let val = self.get_const(&c.0);
        let src = c.0.ety.1.as_int_ty().expect("not casting from int type");
        let n = self.consts.value(val);
        let n = match src {
          IntTy::Int(Size::S8) => i64::from(n as i8) as u64,
          IntTy::Int(Size::S16) => i64::from(n as i16) as u64,
          IntTy::Int(Size::S32) => i64::from(n as i32) as u64,
          _ => n
        };
        let n = match c.1.size() {
          Size::S8 => (n as u8).into(),
          Size::S16 => (n as u16).into(),
          Size::S32 => (n as u32).into(),
          _ => n
        };
        (c.1.size().bytes().expect("impossible").into(), ConstRef::Value(n))
      }
    }
  }

  fn get_operand(&mut self, o: &Operand) -> Result<(RegMemImm<u64>, cl::Operand), GhostErr> {
    Ok(match o.place() {
      Ok(p) => {
        let (p, cl) = self.get_place(p)?;
        (p.into(), cl::Operand::Place(cl))
      }
      Err(c) => match self.get_const(c).1 {
        ConstRef::Value(val) => (val.into(), cl::Operand::Const(cl::Const::Value)),
        ConstRef::Ptr(addr) => (AMode::const_(addr).into(), cl::Operand::Const(cl::Const::Ptr)),
      }
    })
  }

  fn get_operand_reg(&mut self, o: &Operand, sz: Size) -> Result<(VReg, cl::OperandReg), GhostErr> {
    let (o, cl) = self.get_operand(o)?;
    let (reg, cl2) = o.into_reg(&mut self.code, sz);
    Ok((reg, (cl, cl2)))
  }

  fn get_operand_rm(&mut self, o: &Operand, sz: Size) -> Result<(RegMem, cl::OperandRM), GhostErr> {
    let (o, cl) = self.get_operand(o)?;
    let (rm, cl2) = o.into_rm(&mut self.code, sz);
    Ok((rm, (cl, cl2)))
  }

  fn get_operand_32(&mut self, o: &Operand) -> Result<(RegMemImm, cl::Operand32), GhostErr> {
    let (o, cl) = self.get_operand(o)?;
    let (rmi, cl2) = o.into_rmi_32(&mut self.code);
    Ok((rmi, (cl, cl2)))
  }

  fn build_shift_or_zero(&mut self,
    sz: Size, dst: RegMem, kind: ShiftKind, o1: &Operand, o2: &Operand
  ) -> Result<(cl::RValue, Option<VRegRename>), GhostErr> {
    let bits = sz.bits().expect("unbounded");
    let (src1, cl1) = self.get_operand_reg(o1, sz)?;
    let (cl, r) = match self.get_operand(o2)? {
      (RegMemImm::Imm(n), _) => {
        if n >= bits.into() {
          let _ = self.code.emit_copy(sz, dst, 0_u64);
          return Ok((cl::RValue::Shift(cl1, cl::Shift::Zero), None))
        }
        #[allow(clippy::cast_possible_truncation)]
        let temp = self.code.emit_shift(sz, kind, src1, Ok(n as u8));
        let (cl, r) = self.code.emit_copy(sz, dst, temp);
        (cl::Shift::Imm(cl), r)
      }
      (src2, cl2) => {
        let (src2, cl3) = src2.into_reg(&mut self.code, sz);
        let temp = self.code.emit_shift(sz, kind, src1, Err(src2));
        let zero = self.code.emit_imm(sz, 0_u32);
        let cond = self.code.emit_cmp(sz, Cmp::Cmp, CC::NB, src2, u32::from(bits));
        let temp = cond.select(sz, zero, temp);
        let (cl, r) = self.code.emit_copy(sz, dst, temp);
        (cl::Shift::Var((cl2, cl3), cl), r)
      }
    };
    Ok((cl::RValue::Shift(cl1, cl), r))
  }

  fn build_binop(&mut self,
    sz: Size, dst: RegMem, op: VBinop, o1: &Operand, o2: &Operand
  ) -> Result<(cl::RValue, Option<VRegRename>), GhostErr> {
    if sz == Size::Inf { return Err(GhostErr::InfiniteOp) }
    let (src1, cl1) = self.get_operand_reg(o1, sz)?;
    let (src2, cl2) = self.get_operand_32(o2)?;
    let temp = self.code.emit_binop(sz, op, src1, src2);
    let (cl3, r) = self.code.emit_copy(sz, dst, temp);
    Ok((cl::RValue::Binop(cl1, cl2, cl3), r))
}

  fn build_cmp(&mut self,
    sz: Size, dst: RegMem, cc: CC, o1: &Operand, o2: &Operand
  ) -> Result<(cl::RValue, Option<VRegRename>), GhostErr> {
    if sz == Size::Inf { return Err(GhostErr::InfiniteOp) }
    let (src1, cl1) = self.get_operand_reg(o1, sz)?;
    let (src2, cl2) = self.get_operand_32(o2)?;
    let temp = self.code.emit_cmp(sz, Cmp::Cmp, cc, src1, src2).into_reg();
    let (cl3, r) = self.code.emit_copy(Size::S8, dst, temp);
    Ok((cl::RValue::Cmp(cl1, cl2, cl3), r))
  }

  fn build_as(&mut self, dst: RegMem, from: IntTy, to: IntTy, o: &Operand
  ) -> Result<(cl::OperandRM, cl::As, Option<VRegRename>), GhostErr> {
    let sz = from.size().min(to.size());
    if sz == Size::Inf { return Err(GhostErr::InfiniteOp) }
    let (src, cl1) = self.get_operand_rm(o, sz)?;
    let (cl, r) = match ExtMode::new(sz, to.size()) {
      None => {
        let (cl, r) = self.code.emit_copy(sz, dst, src);
        (cl::As::Truncate(cl), r)
      }
      Some(ext_mode) => {
        let temp = self.code.fresh_vreg();
        self.code.emit(match to.signed() {
          true => Inst::MovsxRmR { ext_mode, dst: temp, src },
          false => Inst::MovzxRmR { ext_mode, dst: temp, src },
        });
        let (cl, r) = self.code.emit_copy(to.size(), dst, temp);
        (cl::As::Extend(cl), r)
      }
    };
    Ok((cl1, cl, r))
  }

  fn build_memcpy(&mut self,
    _tysize: u64, sz: Size, dst: RegMem, src: AMode
  ) -> (cl::Copy, Option<VRegRename>) {
    if sz == Size::Inf {
      unimplemented!("large copy");
    } else {
      self.code.emit_copy(sz, dst, src)
    }
  }

  fn build_move(&mut self,
    _tysize: u64, sz: Size, dst: RegMem, o: &Operand
  ) -> Result<(cl::Move, Option<VRegRename>), GhostErr> {
    if sz == Size::Inf {
      unimplemented!("large copy");
    } else {
      let (src, cl1) = self.get_operand(o)?;
      let (cl2, r) = self.code.emit_copy(sz, dst, src);
      Ok((cl::Move::Small(cl1, cl2), r))
    }
  }

  fn build_rvalue(&mut self,
    ty: &TyKind, tysize: u64, sz: Size, dst: RegMem, rv: &RValue
  ) -> Result<(cl::RValue, Option<VRegRename>), GhostErr> {
    Ok(match rv {
      RValue::Use(o) => {
        let (cl, r) = self.build_move(tysize, sz, dst, o)?;
        (cl::RValue::Use(cl), r)
      },
      RValue::Unop(Unop::Not, o) => {
        assert_ne!(sz, Size::Inf);
        let (src, cl1) = self.get_operand_reg(o, sz)?;
        let temp = self.code.emit_binop(sz, VBinop::Xor, src, 1);
        let (cl2, r) = self.code.emit_copy(sz, dst, temp);
        (cl::RValue::Unop(cl1, cl2), r)
      }
      RValue::Unop(Unop::Neg(_), o) => {
        assert_ne!(sz, Size::Inf);
        let (src, cl1) = self.get_operand_reg(o, sz)?;
        let temp = self.code.emit_unop(sz, VUnop::Neg, src);
        let (cl2, r) = self.code.emit_copy(sz, dst, temp);
        (cl::RValue::Unop(cl1, cl2), r)
      }
      RValue::Unop(Unop::BitNot(_), o) => {
        assert_ne!(sz, Size::Inf);
        let (src, cl1) = self.get_operand_reg(o, sz)?;
        let temp = self.code.emit_unop(sz, VUnop::Not, src);
        let (cl2, r) = self.code.emit_copy(sz, dst, temp);
        (cl::RValue::Unop(cl1, cl2), r)
      }
      &RValue::Unop(Unop::As(from, to), ref o) => {
        let (cl1, cl2, r) = self.build_as(dst, from, to, o)?;
        (cl::RValue::As(cl1, cl2), r)
      }
      RValue::Binop(Binop::Add(ity), o1, o2) =>
        self.build_binop(ity.size(), dst, VBinop::Add, o1, o2)?,
      RValue::Binop(Binop::Mul(ity), o1, o2) => {
        let sz = ity.size(); assert_ne!(sz, Size::Inf);
        let (src1, cl1) = self.get_operand_reg(o1, sz)?;
        let (src2, cl2) = self.get_operand_rm(o2, sz)?;
        let temp = self.code.emit_mul(sz, src1, src2).0;
        let (cl3, r) = self.code.emit_copy(sz, dst, temp);
        (cl::RValue::Mul(cl1, cl2, cl3), r)
      }
      RValue::Binop(Binop::Sub(ity), o1, o2) =>
        self.build_binop(ity.size(), dst, VBinop::Sub, o1, o2)?,
      RValue::Binop(Binop::Max(ity), o1, o2) => {
        let sz = ity.size(); assert_ne!(sz, Size::Inf);
        let (src1, cl1) = self.get_operand_reg(o1, sz)?;
        let (src2, cl2) = self.get_operand_reg(o2, sz)?;
        let cc = if ity.signed() { CC::LE } else { CC::BE };
        let cond = self.code.emit_cmp(sz, Cmp::Cmp, cc, src1, src2);
        let temp = cond.select(sz, src2, src1);
        let (cl3, r) = self.code.emit_copy(sz, dst, temp);
        (cl::RValue::Max(cl1, cl2, cl3), r)
      }
      RValue::Binop(Binop::Min(ity), o1, o2) => {
        let sz = ity.size(); assert_ne!(sz, Size::Inf);
        let (src1, cl1) = self.get_operand_reg(o1, sz)?;
        let (src2, cl2) = self.get_operand_reg(o2, sz)?;
        let cc = if ity.signed() { CC::LE } else { CC::BE };
        let cond = self.code.emit_cmp(sz, Cmp::Cmp, cc, src1, src2);
        let temp = cond.select(sz, src1, src2);
        let (cl3, r) = self.code.emit_copy(sz, dst, temp);
        (cl::RValue::Min(cl1, cl2, cl3), r)
      }
      RValue::Binop(Binop::And, o1, o2) =>
        self.build_binop(Size::S8, dst, VBinop::And, o1, o2)?,
      RValue::Binop(Binop::Or, o1, o2) =>
        self.build_binop(Size::S8, dst, VBinop::Or, o1, o2)?,
      RValue::Binop(Binop::BitAnd(ity), o1, o2) =>
        self.build_binop(ity.size(), dst, VBinop::And, o1, o2)?,
      RValue::Binop(Binop::BitOr(ity), o1, o2) =>
        self.build_binop(ity.size(), dst, VBinop::Or, o1, o2)?,
      RValue::Binop(Binop::BitXor(ity), o1, o2) =>
        self.build_binop(ity.size(), dst, VBinop::Xor, o1, o2)?,
      RValue::Binop(Binop::Shl(_), o1, o2) =>
        self.build_shift_or_zero(sz, dst, ShiftKind::Shl, o1, o2)?,
      RValue::Binop(Binop::Shr(ity), o1, o2) => {
        let kind = if ity.signed() { ShiftKind::ShrA } else { ShiftKind::ShrL };
        self.build_shift_or_zero(sz, dst, kind, o1, o2)?
      }
      RValue::Binop(Binop::Lt(ity), o1, o2) =>
        self.build_cmp(ity.size(), dst, if ity.signed() { CC::L } else { CC::B }, o1, o2)?,
      RValue::Binop(Binop::Le(ity), o1, o2) =>
        self.build_cmp(ity.size(), dst, if ity.signed() { CC::LE } else { CC::BE }, o1, o2)?,
      RValue::Binop(Binop::Eq(ity), o1, o2) => self.build_cmp(ity.size(), dst, CC::Z, o1, o2)?,
      RValue::Binop(Binop::Ne(ity), o1, o2) => self.build_cmp(ity.size(), dst, CC::NZ, o1, o2)?,
      &RValue::Eq(ref ty, invert, ref o1, ref o2) => {
        let meta = ty.meta(self.names).expect("size of type not a compile time constant");
        let sz = Size::from_u64(meta.size);
        if meta.on_stack {
          unimplemented!("memcmp")
        } else {
          self.build_cmp(sz, dst, if invert { CC::NZ } else { CC::Z }, o1, o2)?
        }
      }
      RValue::Pun(..) => unreachable!("handled in build()"),
      RValue::Cast(_, o, tyin) =>
        if let (Some(from), Some(to)) = (tyin.as_int_ty(), ty.as_int_ty()) {
          let (cl1, cl2, r) = self.build_as(dst, from, to, o)?;
          (cl::RValue::Cast(cl1, cl2), r)
        } else {
          unimplemented!("casting between non-integral types: {:?} -> {:?}", tyin, ty)
        },
      RValue::List(os) => {
        let TyKind::Struct(args) = ty else { unreachable!() };
        assert_eq!(args.len(), os.len());
        let mut rm = dst;
        let mut rename = None;
        let mut last_off = 0;
        for (arg, o) in args.iter().zip(&**os) {
          let elem = if arg.attr.contains(ArgAttr::GHOST) {
            cl::Elem::Ghost
          } else {
            let sz = arg.ty.sizeof(self.names)
              .expect("struct element size not known at compile time");
            if sz == 0 {
              cl::Elem::Ghost
            } else {
              if last_off != 0 {
                match &mut rm {
                  RegMem::Reg(_) => panic!("register should be address-taken"),
                  RegMem::Mem(a) => *a = &*a + u32::try_from(last_off).expect("overflow")
                }
              }
              last_off = sz;
              let (cl, r) = self.build_move(sz, Size::from_u64(sz), rm, o)?;
              rename = r;
              cl::Elem::Move(cl)
            }
          };
          self.code.trace.lists.push(elem);
        }
        (cl::RValue::List(os.len().try_into().expect("overflow")), rename)
      }
      RValue::Array(os) => if let [ref o] = **os {
        let (cl, r) = self.build_move(tysize, sz, dst, o)?;
        (cl::RValue::Array1(cl), r)
      } else {
        let TyKind::Array(ty, _) = ty else { unreachable!() };
        let sz64 = ty.sizeof(self.names).expect("impossible");
        if sz64 == 0 {
          (cl::RValue::Ghost, None)
        } else {
          let sz32 = u32::try_from(sz64).expect("overflow");
          let sz = Size::from_u64(sz64);
          let mut a = match dst {
            RegMem::Reg(_) => panic!("register should be address-taken"),
            RegMem::Mem(a) => a
          };
          for o in &**os {
            let elem = cl::Elem::Move(self.build_move(sz64, sz, RegMem::Mem(a), o)?.0);
            a = &a + sz32;
            self.code.trace.lists.push(elem);
          }
          (cl::RValue::Array(os.len().try_into().expect("overflow")), None)
        }
      }
      RValue::Ghost(_) |
      RValue::Mm0(..) |
      RValue::Typeof(_) => (cl::RValue::Ghost, None),
      RValue::Borrow(p) => {
        let (rm, cl1) = self.get_place(p)?;
        let temp = match rm {
          RegMem::Reg(_) => panic!("register should be address-taken"),
          RegMem::Mem(a) => self.code.emit_lea(Size::S64, a),
        };
        let (cl2, r) = self.code.emit_copy(sz, dst, temp);
        (cl::RValue::Borrow(cl1, cl2), r)
      }
      RValue::GetArgc => {
        assert_eq!(sz, Size::S64);
        let addr = AMode {
          off: Offset::Rsp(0),
          base: VReg::invalid(),
          si: None,
        };
        let (cl, r) = self.build_memcpy(tysize, sz, dst, addr);
        (cl::RValue::GetArgc(cl), r)
      }
      RValue::GetArgv => {
        assert_eq!(sz, Size::S64);
        let addr = AMode {
          off: Offset::Rsp(8),
          base: VReg::invalid(),
          si: None,
        };
        let (cl, r) = self.build_memcpy(tysize, sz, dst, addr);
        (cl::RValue::GetArgv(cl), r)
      }
    })
  }

  fn build_jump(&mut self,
    vbl: VBlockId,
    block_args: &ChunkVec<BlockId, VReg>,
    tgt: BlockId,
    args: &[(VarId, bool, Operand)]
  ) -> Result<cl::Terminator, GhostErr> {
    let params = &block_args[tgt];
    let mut params_it = params.iter().peekable();
    for &(v, r, ref o) in args {
      let cl = if r {
        let a = self.allocs.get(v);
        assert_ne!(a, AllocId::ZERO);
        let (&(dst, sz), size) = self.get_alloc(a);
        if let RegMem::Reg(v) = dst {
          if params_it.peek() == Some(&&v) { params_it.next(); }
        }
        let (cl, r) = self.build_move(size, sz, dst, o)?;
        if let Some(r) = r { self.rename_alloc(a, r); }
        cl::Elem::Move(cl)
      } else {
        cl::Elem::Ghost
      };
      self.code.trace.lists.push(cl);
    }
    assert!(params_it.peek().is_none());
    self.unpatched.push((vbl, self.code.emit(Inst::JmpKnown {
      dst: VBlockId(tgt.0),
      params: params.iter().map(|v| v.0).collect()
    })));
    Ok(cl::Terminator::Jump(args.len().try_into().expect("overflow")))
  }

  fn build_ret(&mut self, args: &[(VarId, bool, Operand)]) -> Result<(), GhostErr> {
    assert!(self.can_return);
    assert_eq!(args.len(), self.abi_rets.len());
    let incoming = AMode::spill(SpillId::INCOMING);
    let mut params = vec![];
    for (&(_, r, ref o), ret) in args.iter().zip(&*self.abi_rets.clone()) {
      assert!(r || matches!(ret, VRetAbi::Ghost));
      let cl = match *ret {
        VRetAbi::Ghost => cl::Elem::Ghost,
        VRetAbi::Reg(reg, sz) => {
          let mut dst = self.code.fresh_vreg();
          let (src, cl1) = self.get_operand(o)?;
          let (_, r) = self.code.emit_copy(sz, dst.into(), src);
          if let Some(r) = r { dst = dst.rename(r) }
          params.push(ROperand::reg_fixed_use(dst.0, reg.0));
          cl::Elem::Operand(cl1)
        }
        VRetAbi::Mem { off, sz } => {
          let sz = sz.into();
          cl::Elem::Move(self.build_move(sz, Size::from_u64(sz), (&incoming + off).into(), o)?.0)
        }
        VRetAbi::Boxed { reg: (dst, _), sz } => {
          let sz = sz.into();
          cl::Elem::Move(self.build_move(sz, Size::from_u64(sz), AMode::reg(dst).into(), o)?.0)
        }
        VRetAbi::BoxedMem { off, sz } => {
          let ptr = self.code.fresh_vreg();
          self.code.emit(Inst::load_mem(Size::S64, ptr, &incoming + off));
          let sz = sz.into();
          cl::Elem::Move(self.build_move(sz, Size::from_u64(sz), AMode::reg(ptr).into(), o)?.0)
        }
      };
      self.code.trace.lists.push(cl);
    }
    self.code.emit(Inst::Epilogue { params: params.into() });
    Ok(())
  }

  fn build_call(&mut self,
    vbl: VBlockId,
    f: ProcId,
    args: &[(bool, Operand)],
    reach: bool,
    tgt: BlockId,
    rets: &[(bool, VarId)],
  ) -> Result<cl::Terminator, GhostErr> {
    let fabi = &self.funcs[f];
    assert!(fabi.args.len() == args.len());
    let outgoing = AMode::spill(SpillId::OUTGOING);
    let mut operands = vec![];
    for (arg, &(r, ref o)) in fabi.args.iter().zip(args) {
      let cl = if r {
        match *arg {
          ArgAbi::Ghost => cl::Elem::Ghost,
          ArgAbi::Reg(reg, sz) => {
            let (src, cl1) = self.get_operand(o)?;
            let mut temp = self.code.fresh_vreg();
            let (_, r) = self.code.emit_copy(sz, temp.into(), src);
            if let Some(r) = r { temp = temp.rename(r) }
            operands.push(ROperand::reg_fixed_use(temp.0, reg.0));
            cl::Elem::Operand(cl1)
          }
          ArgAbi::Mem { off, sz } => {
            let sz64 = sz.into();
            cl::Elem::Move(
              self.build_move(sz64, Size::from_u64(sz64), (&outgoing + off).into(), o)?.0)
          }
          ArgAbi::Boxed { reg, sz } => {
            let sz64 = sz.into();
            let sz = Size::from_u64(sz64);
            let (o, cl1) = self.get_operand(o)?;
            let (src, cl2) = o.into_mem(&mut self.code, sz);
            let temp = self.code.emit_lea(Size::S64, src);
            operands.push(ROperand::reg_fixed_use(temp.0, reg.0));
            cl::Elem::Boxed(cl1, cl2)
          }
          ArgAbi::BoxedMem { off, sz } => {
            let sz64 = sz.into();
            let sz = Size::from_u64(sz64);
            let (o, cl1) = self.get_operand(o)?;
            let (src, cl2) = o.into_mem(&mut self.code, sz);
            let temp = self.code.emit_lea(Size::S64, src);
            let _ = self.code.emit_copy(Size::S64, (&outgoing + off).into(), temp);
            cl::Elem::BoxedMem(cl1, cl2)
          }
        }
      } else {
        cl::Elem::Ghost
      };
      self.code.trace.lists.push(cl)
    }
    if reach {
      assert!(fabi.rets.len() == rets.len());
      let mut boxes = vec![];
      let mut ret_regs = vec![];
      for (arg, &(vr, v)) in fabi.rets.iter().zip(rets) {
        if !vr { continue }
        if let ArgAbi::Reg(reg, _) = *arg {
          let src = self.code.fresh_vreg();
          operands.push(ROperand::reg_fixed_def(src.0, reg.0));
          ret_regs.push(src);
        }
        if let ArgAbi::Boxed {..} | ArgAbi::BoxedMem {..} = arg {
          let a = self.allocs.get(v);
          assert_ne!(a, AllocId::ZERO);
          let (&(dst, sz), size) = self.get_alloc(a);
          let (addr, cl) = match dst {
            RegMem::Reg(r) => {
              let am = AMode::spill(self.code.fresh_spill(
                size.try_into().expect("allocation too large")));
              boxes.push((sz, a, r, am));
              (am, true)
            }
            RegMem::Mem(a) => (a, false),
          };
          let temp = self.code.emit_lea(Size::S64, addr);
          match *arg {
            ArgAbi::Boxed { reg, .. } =>
              operands.push(ROperand::reg_fixed_use(temp.0, reg.0)),
            ArgAbi::BoxedMem { off, .. } => {
              let _ = self.code.emit_copy(Size::S64, (&outgoing + off).into(), temp);
            }
            _ => unreachable!()
          }
          self.code.trace.lists.push(cl::Elem::RetArg(cl::IntoMem(cl)))
        }
      }
      self.emit(Inst::CallKnown {
        f,
        operands: operands.into(),
        clobbers: Some(fabi.clobbers),
      });
      let mut ret_regs = ret_regs.into_iter();
      for (arg, &(vr, v)) in fabi.rets.iter().zip(rets) {
        if !vr { continue }
        let a = self.allocs.get(v);
        assert_ne!(a, AllocId::ZERO);
        let dst = self.get_alloc(a).0.0;
        let (cl, r) = match *arg {
          ArgAbi::Reg(_, sz) => {
            let (_, r) = self.code.emit_copy(sz, dst, ret_regs.next().expect("pushed"));
            (cl::Elem::RetReg, r)
          }
          ArgAbi::Mem { off, sz } => {
            let sz64 = sz.into();
            let (cl, r) = self.build_memcpy(sz64, Size::from_u64(sz64), dst, &outgoing + off);
            (cl::Elem::RetMem(cl), r)
          }
          _ => (cl::Elem::Ghost, None)
        };
        if let Some(r) = r { self.rename_alloc(a, r); }
        self.code.trace.lists.push(cl)
      }
      for (sz, a, dst, am) in boxes {
        let (_, r) = self.code.emit_copy(sz, dst.into(), am);
        if let Some(r) = r { self.rename_alloc(a, r) }
      }
      self.unpatched.push((vbl, self.code.emit(Inst::Fallthrough {
        dst: VBlockId(tgt.0),
      })));
    } else {
      assert!(!fabi.reach);
      self.emit(Inst::CallKnown { f, operands: operands.into(), clobbers: None });
    }
    Ok(cl::Terminator::Call(f))
  }

  fn build_intrinsic(&mut self,
    vbl: VBlockId,
    intrinsic: IntrinsicProc,
    args: &[(bool, Operand)],
    tgt: BlockId,
    rets: &[(bool, VarId)],
  ) -> Result<cl::Terminator, GhostErr> {
    const CV: cl::Operand = cl::Operand::Const(cl::Const::Value);
    let mut rmis = ArrayVec::<(RegMemImm<u64>, cl::Operand), 6>::new();
    let (f, (ret_used, ret)) = match (intrinsic, rets, args) {
      (IntrinsicProc::Open, &[ret], [(true, fname)]) => {
        rmis.extend([self.get_operand(fname)?, (0.into(), CV), (0.into(), CV)]);
        (SysCall::Open, ret)
      }
      (IntrinsicProc::Create, &[ret], [(true, fname)]) => {
        rmis.extend([self.get_operand(fname)?, ((1 + (1<<6) + (1<<9)).into(), CV), (0.into(), CV)]);
        (SysCall::Open, ret)
      }
      (IntrinsicProc::Read, &[ret], [(true, fd), (true, count), (_, _buf), (true, p)]) => {
        rmis.extend([self.get_operand(fd)?, self.get_operand(p)?, self.get_operand(count)?]);
        (SysCall::Read, ret)
      }
      (IntrinsicProc::Write, &[ret], [(true, fd), (true, count), (_, _buf), (true, p)]) => {
        rmis.extend([self.get_operand(fd)?, self.get_operand(p)?, self.get_operand(count)?]);
        (SysCall::Write, ret)
      }
      (IntrinsicProc::FStat, &[(_, _buf_new), ret], [(true, fd), (_, _buf_old), (true, p)]) => {
        rmis.extend([self.get_operand(fd)?, self.get_operand(p)?]);
        (SysCall::FStat, ret)
      }
      (IntrinsicProc::MMap, &[ret], [(true, len), (true, prot), (true, fd)]) => {
        rmis.extend([
          (0.into(), CV),
          self.get_operand(len)?,
          self.get_operand(prot)?,
          (2.into(), CV),
          self.get_operand(fd)?,
          (0.into(), CV),
        ]);
        (SysCall::MMap, ret)
      }
      (IntrinsicProc::MMapAnon, &[ret], [(true, len), (true, prot)]) => {
        rmis.extend([
          (0.into(), CV),
          self.get_operand(len)?,
          self.get_operand(prot)?,
          ((2+32).into(), CV),
          (u64::from(u32::MAX).into(), CV),
          (0.into(), CV),
        ]);
        (SysCall::MMap, ret)
      }
      e => panic!("intrinsic has the wrong number of arguments: {e:?}")
    };
    let vreg = self.code.fresh_vreg();
    self.build_syscall(f, &rmis, vreg);
    let cl2 = if ret_used {
      let a = self.allocs.get(ret);
      assert_ne!(a, AllocId::ZERO);
      let (dst, sz) = *self.get_alloc(a).0;
      let (cl, r) = self.code.emit_copy(sz, dst, vreg);
      if let Some(r) = r { self.rename_alloc(a, r); }
      Some(cl)
    } else {
      None
    };
    self.unpatched.push((vbl, self.code.emit(Inst::Fallthrough { dst: VBlockId(tgt.0) })));
    Ok(cl::Terminator::Intrinsic(intrinsic, cl2))
  }

  fn build_syscall(&mut self, f: SysCall, args: &[(RegMemImm<u64>, cl::Operand)], dst: VReg) {
    let (rax, ref argregs) = SYSCALL_ARG_REGS;
    debug_assert!(args.len() <= argregs.len());
    let fname = self.code.fresh_vreg();
    let _ = self.code.emit_copy(Size::S32, fname.into(), u64::from(f as u8));
    let mut params = vec![ROperand::reg_fixed_use(fname.0, rax.0)];
    for ((arg, cl), &reg) in args.iter().zip(argregs) {
      let mut dst = self.code.fresh_vreg();
      let (_, r) = self.code.emit_copy(Size::S64, dst.into(), *arg);
      if let Some(r) = r { dst = dst.rename(r) }
      params.push(ROperand::reg_fixed_use(dst.0, reg.0));
      self.code.trace.lists.push(cl::Elem::Operand(*cl))
    }
    if f.returns() { params.push(ROperand::reg_fixed_def(dst.0, rax.0)) }
    self.code.emit(Inst::SysCall { f, operands: params.into() });
  }

  fn build_terminator(&mut self,
    block_args: &ChunkVec<BlockId, VReg>, vbl: VBlockId, term: &Terminator
  ) -> Result<cl::Terminator, GhostErr> {
    Ok(match *term {
      Terminator::Jump(tgt, ref args, _) => self.build_jump(vbl, block_args, tgt, args)?,
      Terminator::Jump1(_, tgt) => {
        self.unpatched.push((vbl, self.code.emit(Inst::Fallthrough {
          dst: VBlockId(tgt.0)
        })));
        cl::Terminator::Jump1
      }
      Terminator::Return(_, ref args) => {
        self.build_ret(args)?;
        cl::Terminator::Return
      }
      Terminator::Exit(_) => {
        let dst = self.code.fresh_vreg();
        self.build_syscall(SysCall::Exit, &[(0.into(), cl::Operand::Const(cl::Const::Value))], dst);
        cl::Terminator::Exit
      }
      Terminator::If(_, ref o, [(_, bl1), (_, bl2)]) => {
        let (src, cl) = self.get_operand_reg(o, Size::S8)?;
        let cond = self.code.emit_cmp(Size::S8, Cmp::Cmp, CC::NZ, src, 0_u32);
        self.unpatched.push((vbl, cond.branch(VBlockId(bl1.0), VBlockId(bl2.0))));
        cl::Terminator::If(cl)
      }
      Terminator::Assert(ref o, _, bl) => {
        let (src, cl1) = self.get_operand_reg(o, Size::S8)?;
        let cond = self.code.emit_cmp(Size::S8, Cmp::Cmp, CC::NZ, src, 0_u32);
        self.unpatched.push((vbl, cond.assert(VBlockId(bl.0))));
        cl::Terminator::Assert(cl1)
      }
      Terminator::Fail => {
        self.code.emit(Inst::Ud2);
        cl::Terminator::Fail
      }
      Terminator::Call { f, ref tys, ref args, reach, tgt, ref rets, .. } => {
        if !tys.is_empty() { unimplemented!("monomorphization") }
        if let Some(&f) = self.func_mono.get(&f) {
          self.build_call(vbl, f, args, reach, tgt, rets)?
        } else if let Some(&Entity::Proc(Spanned {
          k: ProcTc::Typed(ProcTy {intrinsic: Some(intrinsic), ..}), ..
        })) = self.names.get(&f) {
          self.build_intrinsic(vbl, intrinsic, args, tgt, rets)?
        } else {
          panic!("function ABI not found");
        }
      }
      Terminator::Unreachable(_) |
      Terminator::Dead => unreachable!(),
    })
  }

  fn build_block_args(&mut self) -> Result<ChunkVec<BlockId, VReg>, LowerErr> {
    let preds = self.cfg.predecessors();

    let cfg = self.cfg;
    let mut insert = |out: &mut Vec<_>, v| {
      let a = self.allocs.get(v);
      if a == AllocId::ZERO { return Err(v) }
      if let RegMem::Reg(v) = self.get_alloc(a).0.0 {
        if !out.contains(&v) { out.push(v) }
      }
      Ok(())
    };

    let mut block_args = ChunkVec::default();
    for (i, bl) in cfg.blocks.enum_iter() {
      let mut out = vec![];
      if i != BlockId::ENTRY && !bl.is_dead() {
        (|| -> Result<_, VarId> {
          for &(e, j) in &preds[i] {
            if !matches!(e, Edge::Jump | Edge::Call) { continue }
            match cfg[j].terminator() {
              Terminator::Jump(_, args, _) =>
                for &(v, r, _) in &**args { if r { insert(&mut out, v)? } }
              Terminator::Call {rets, ..} =>
                for &(r, v) in &**rets { if r { insert(&mut out, v)? } }
              _ => unreachable!()
            }
          }
          Ok(())
        })().map_err(|v| LowerErr::GhostVarUsed({
          bl.ctx_rev_iter(&cfg.ctxs).find(|p| p.0.k == v)
            .unwrap_or_else(|| unreachable!("missing variable {:?}", v)).0.clone()
        }))?
      }
      block_args.push(out);
    }
    Ok(block_args)
  }

  fn build_prologue(&mut self, bl: &'a BasicBlock, ctx: VCodeCtx<'_>) {
    let mut arg_regs = RET_AND_ARG_REGS.iter();
    let incoming = AMode::spill(SpillId::INCOMING);
    let mut off = 0_u32;
    let mut alloc = |sz| {
      let old = off;
      off = off.checked_add(sz).expect("overflow");
      old
    };

    if let VCodeCtx::Proc(rets) = ctx {
      self.abi_rets = rets.iter().map(|ret| {
        if ret.attr.contains(ArgAttr::GHOST) { return VRetAbi::Ghost }
        let meta = ret.ty.meta(self.names).expect("return must have compile time known size");
        let size = meta.size;
        let sz = Size::from_u64(size);
        let on_stack = meta.on_stack || sz == Size::Inf;
        match (on_stack, arg_regs.next()) {
          (false, Some(&r)) => VRetAbi::Reg(r, sz),
          (true, Some(&r)) => {
            let ptr = self.code.fresh_vreg();
            self.code.emit(Inst::MovPR { dst: ptr, src: r });
            VRetAbi::Boxed { reg: (ptr, r), sz: size.try_into().expect("overflow") }
          }
          (_, None) if size <= 8 => {
            let size32 = size.try_into().expect("overflow");
            VRetAbi::Mem { off: alloc(size32), sz: size32 }
          },
          (_, None) => VRetAbi::BoxedMem { off: alloc(8), sz: size.try_into().expect("overflow") }
        }
      }).collect();
    }

    self.abi_args = bl.ctx_iter(&self.cfg.ctxs).map(|(v, b, _)| {
      if !b { return ArgAbi::Ghost }
      let a = self.allocs.get(v.k);
      assert_ne!(a, AllocId::ZERO);
      let (&(dst, sz), size) = self.get_alloc(a);
      let (abi, r) = match (dst, arg_regs.next()) {
        (RegMem::Reg(dst), Some(&r)) => {
          self.code.emit(Inst::MovPR { dst, src: r });
          (ArgAbi::Reg(r, sz), None)
        },
        (RegMem::Mem(_), Some(&reg)) => {
          let src = self.code.fresh_vreg();
          self.code.emit(Inst::MovPR { dst: src, src: reg });
          let size32 = size.try_into().expect("overflow");
          let (cl, r) = self.build_memcpy(size, sz, dst, AMode::reg(src));
          self.code.trace.lists.push(cl::Elem::ArgCopy(cl));
          (ArgAbi::Boxed { reg, sz: size32 }, r)
        },
        (_, None) if size <= 8 => {
          let size32 = size.try_into().expect("overflow");
          let off = alloc(size32);
          let (cl, r) = self.build_memcpy(size, sz, dst, &incoming + off);
          self.code.trace.lists.push(cl::Elem::ArgCopy(cl));
          (ArgAbi::Mem { off, sz: size32 }, r)
        },
        (_, None) => {
          let off = alloc(8);
          let mut ptr = self.code.fresh_vreg();
          let (_, r) = self.code.emit_copy(Size::S64, ptr.into(), &incoming + off);
          if let Some(r) = r { ptr = ptr.rename(r) }
          let (cl, r) = self.build_memcpy(size, sz, dst, AMode::reg(ptr));
          self.code.trace.lists.push(cl::Elem::ArgCopy(cl));
          (ArgAbi::BoxedMem { off, sz: size.try_into().expect("overflow") }, r)
        },
      };
      if let Some(r) = r { self.rename_alloc(a, r); }
      abi
    }).collect();

    self.code.grow_spill(SpillId::INCOMING, off);
  }

  fn build_block(&mut self,
    block_args: &ChunkVec<BlockId, VReg>, bl: &'a BasicBlock, vblock: VBlockId
  ) -> Result<(), GhostErr> {
    self.code.trace.stmts.push_new();
    let proj_start = self.code.trace.projs.len().try_into().expect("overflow");
    let list_start = self.code.trace.lists.len().try_into().expect("overflow");
    for stmt in &bl.stmts {
      let cl = if stmt.relevant() {
        match stmt {
          Statement::Let(lk, _, ty, rv) => {
            let ((LetKind::Let(v, _), ty) |
              (LetKind::Ptr([_, (v, ty)]), _)) = (lk, ty);
            let a = self.allocs.get(v.k);
            assert_ne!(a, AllocId::ZERO);
            if let RValue::Pun(_, p) = rv {
              let (rm, cl) = self.get_place(p)?;
              self.var_map.entry(a).or_insert_with(||
                (rm, Size::from_u64(self.allocs[a].m.size)));
              cl::Statement::Let(cl::RValue::Pun(cl))
            } else {
              let (&(dst, sz), size) = self.get_alloc(a);
              // self.code.emit(Inst::LetStart { size: size.try_into().expect("too large") });
              let (cl, r) = self.build_rvalue(ty, size, sz, dst, rv)?;
              // self.code.emit(Inst::LetEnd { dst });
              if let Some(r) = r { self.rename_alloc(a, r); }
              cl::Statement::Let(cl)
            }
          }
          Statement::Assign(p, ty, o, _) => {
            let size = ty.sizeof(self.names).expect("size of type not a compile time constant");
            let (dst, cl) = self.get_place(p)?;
            let (cl2, r) = self.build_move(size, Size::from_u64(size), dst, o)?;
              if let Some(r) = r { self.rename_alloc(self.allocs.get(p.local), r); }
            cl::Statement::Assign(cl, cl2)
          }
          Statement::LabelGroup(..) | Statement::PopLabelGroup |
          Statement::DominatedBlock(..) => cl::Statement::Ghost
        }
      } else {
        cl::Statement::Ghost
      };
      self.code.trace.stmts.extend_last(cl);
      stmt.foreach_def(|v, _, _, ty| self.ctx.insert(v.k, &v.span, ty.clone()));
    }
    let cl = self.build_terminator(block_args, vblock, bl.terminator())?;
    self.code.trace.block.push(cl::Block { proj_start, list_start, term: cl });
    self.code.finish_block();
    Ok(())
  }

  fn build_blocks(&mut self, block_args: &ChunkVec<BlockId, VReg>, ctx: VCodeCtx<'_>) -> Result<(), LowerErr> {
    visit_blocks(self.cfg, move |i, bl| {
      assert!(!bl.is_dead()); // dead blocks are not reachable from the entry
      let vblock = self.code.new_block(i, block_args[i].iter().copied());
      self.code.block_map.insert(i, vblock);
      self.ctx.start_block(bl);
      if i == BlockId::ENTRY { self.build_prologue(bl, ctx) }
      for (v, r, _) in bl.ctx_iter(&self.cfg.ctxs) {
        if !r { continue }
        let a = self.allocs.get(v.k);
        assert_ne!(a, AllocId::ZERO);
        let val = self.get_alloc(a).0.0;
        self.code.emit(Inst::BlockParam { var: v.k, val });
      }
      self.build_block(block_args, bl, vblock).map_err(|err| match err {
        GhostErr::GhostVarUsed(v) => {
          let span = self.ctx.ctx.get(&v)
            .unwrap_or_else(|| unreachable!("missing variable {:?}", v)).0.clone();
          LowerErr::GhostVarUsed(Spanned { span, k: v })
        }
        GhostErr::InfiniteOp => LowerErr::InfiniteOp(self.cfg.span.clone()),
      })
    })
  }

  fn finish(self) -> VCode {
    let LowerCtx { mut code, unpatched, abi_args, abi_rets, can_return, .. } = self;
    macro_rules! patch {($dst:expr) => {{ *$dst = code.block_map[&BlockId($dst.0)]; *$dst }}}
    for (vbl, inst) in unpatched {
      match &mut code.insts[inst] {
        Inst::Fallthrough { dst } |
        Inst::Assert { dst, .. } |
        Inst::JmpKnown { dst, .. } => {
          let dst = patch!(dst);
          code.add_edge(vbl, dst)
        }
        Inst::JmpCond { taken, not_taken, .. } => {
          let (bl1, bl2) = (patch!(taken), patch!(not_taken));
          code.add_edge(vbl, bl1);
          code.add_edge(vbl, bl2);
        }
        _ => unreachable!(),
      }
    }
    code.abi.args = abi_args.into();
    code.abi.rets = abi_rets.iter().map(ArgAbi::from).collect();
    code.abi.reach = can_return;
    code.abi.args_space = code.spills[SpillId::INCOMING];
    code
  }
}

pub(crate) fn build_vcode(
  names: &HashMap<Symbol, Entity>,
  func_mono: &HashMap<Symbol, ProcId>,
  funcs: &IdxVec<ProcId, ProcAbi>,
  consts: &ConstData,
  cfg: &Cfg,
  allocs: &Allocations,
  ctx: VCodeCtx<'_>,
) -> Result<VCode, LowerErr> {
  let mut lctx = LowerCtx::new(names, func_mono, funcs, consts, cfg, allocs, ctx);
  let block_args = lctx.build_block_args()?;
  lctx.build_blocks(&block_args, ctx)?;
  Ok(lctx.finish())
}
