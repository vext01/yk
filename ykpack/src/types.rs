// Copyright 2019 King's College London.
// Created by the Software Development Team <http://soft-dev.org/>.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Types for the Yorick intermediate language.

use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};

pub type CrateHash = u64;
pub type DefIndex = u32;
pub type BasicBlockIndex = u32;
pub type LocalIndex = u32;

/// A mirror of the compiler's notion of a "definition ID".
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct DefId {
    pub crate_hash: CrateHash,
    pub def_idx: DefIndex,
}

impl DefId {
    pub fn new(crate_hash: CrateHash, def_idx: DefIndex) -> Self {
        Self {
            crate_hash,
            def_idx,
        }
    }
}

impl Display for DefId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "DefId({}, {})", self.crate_hash, self.def_idx)
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct Mir {
    pub def_id: DefId,
    pub item_path_str: String,
    pub blocks: Vec<BasicBlock>,
}

impl Mir {
    pub fn new(def_id: DefId, item_path_str: String, blocks: Vec<BasicBlock>) -> Self {
        Self {
            def_id,
            item_path_str,
            blocks,
        }
    }
}

impl Display for Mir {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "[Begin TIR for {}]", self.item_path_str)?;
        writeln!(f, "    {}:", self.def_id)?;
        for (i, b) in self.blocks.iter().enumerate() {
            write!(f, "    bb{}:\n{}", i, b)?;
        }
        writeln!(f, "[End TIR for {}]", self.item_path_str)?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct BasicBlock {
    pub stmts: Vec<Statement>,
    pub term: Terminator,
}

impl BasicBlock {
    pub fn new(stmts: Vec<Statement>, term: Terminator) -> Self {
        Self { stmts, term }
    }
}

impl Display for BasicBlock {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for s in self.stmts.iter() {
            write!(f, "        {}", s)?;
        }
        writeln!(f, "        term: {}\n", self.term)
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Statement {
    Nop,
    /// This is a special instruction used only in SSA generation.
    SsaEntryDefs(Vec<LocalIndex>),
    Assign(Place, Rvalue),
    Unimplemented, // FIXME
}

impl Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "{:?}", self)
    }
}

impl Statement {
    pub fn uses_vars_mut(&mut self) -> Vec<&mut LocalIndex> {
        match self {
            Statement::Nop | Statement::Unimplemented | Statement::SsaEntryDefs(_) => vec![],
            Statement::Assign(_, rv) => rv.uses_vars_mut(),
        }
    }

    pub fn defs_vars_mut(&mut self) -> Vec<&mut LocalIndex> {
        match self {
            Statement::Nop | Statement::Unimplemented => vec![],
            Statement::Assign(p, _) => p.defs_vars_mut(),
            Statement::SsaEntryDefs(ref mut vs) => vs.iter_mut().collect(),
        }
    }

    pub fn is_phi(&self) -> bool {
        if let Statement::Assign(_, Rvalue::Phi(..)) = self {
            return true;
        }
        false
    }

    pub fn phi_arg_mut(&mut self, j: usize) -> Option<&mut LocalIndex> {
        if let Statement::Assign(_, Rvalue::Phi(ps)) = self {
            if let Place::Local(ref mut l) = ps[j] {
                return Some(l);
            }
        }
        None
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Place {
    Local(LocalIndex),
    Unimplemented, // FIXME
}

impl Place {
    fn uses_vars_mut(&mut self) -> Vec<&mut LocalIndex> {
        match self {
            Place::Local(l) => vec![l],
            Place::Unimplemented => vec![],
        }
    }

    fn defs_vars_mut(&mut self) -> Vec<&mut LocalIndex> {
        match self {
            Place::Local(l) => vec![l],
            Place::Unimplemented => vec![],
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Rvalue {
    Place(Place),
    Phi(Vec<Place>),
    Unimplemented, // FIXME
}

impl Rvalue {
    fn uses_vars_mut(&mut self) -> Vec<&mut LocalIndex> {
        match self {
            Rvalue::Place(p) => p.uses_vars_mut(),
            Rvalue::Phi(ps) => {
                let mut res = Vec::new();
                ps.iter_mut().fold(&mut res, |r, p| {
                    r.extend(p.uses_vars_mut());
                    r
                });
                res
            }
            Rvalue::Unimplemented => vec![],
        }
    }
}

// Rvalue operands.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Operand {
    Copy(Place),
    Move(Place),
    Constant(Constant),
}

impl Operand {
    /// Assuming this is a call terminator operand, what is the target DefId?
    pub fn call_operand_defid(&self) -> Option<&DefId> {
        if let Operand::Constant(Constant {
            ty: TyS {
                sty: TyKind::FnDef(def_id), ..
            }, ..
        }) = self {
            return Some(def_id);
        }

        None
    }

    fn uses_vars_mut(&mut self) -> Vec<&mut LocalIndex> {
        match self {
            Operand::Copy(p) | Operand::Move(p) => p.uses_vars_mut(),
            Operand::Constant(..) => vec![],
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct TyS {
    sty: TyKind,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
enum TyKind {
    FnDef(DefId), // FIXME substs not implemented.
    Unimplemented,
}

/// A compile-time constant.
/// Here we deviate from the MIR slightly, as we will never see an unevaluated constant.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct Constant {
    // FIXME in MIR this is a shared reference. We should also share it.
    ty: TyS,
    // FIXME This too is a shared reference in MIR.
    //literal: ConstValue,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
struct Ty {} // FIXME

// #[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
// pub enum ConstValue {
//     Scalar(Scalar),
//     Unimplemented, // FIXME
// }

// An immediate primitive constant value.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Scalar {
    Bits {
        size: u8,
        bits: u128,
    },
    Unimplemented, // FIXME
}

/// A basic block terminator.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Terminator {
    Goto {
        target_bb: BasicBlockIndex,
    },
    SwitchInt {
        discr: Operand,
        target_bbs: Vec<BasicBlockIndex>,
    },
    Resume,
    Abort,
    Return {
        // Because TIR is in SSA, we have to say which SSA variable to return.
        local: LocalIndex
    },
    Unreachable,
    Drop {
        target_bb: BasicBlockIndex,
        unwind_bb: Option<BasicBlockIndex>,
    },
    DropAndReplace {
        target_bb: BasicBlockIndex,
        unwind_bb: Option<BasicBlockIndex>,
    },
    Call {
        func: Operand,
        cleanup_bb: Option<BasicBlockIndex>,
        ret_bb: Option<BasicBlockIndex>,
    },
    Assert {
        cond: Operand,
        target_bb: BasicBlockIndex,
        cleanup_bb: Option<BasicBlockIndex>,
    },
    Yield {
        value: Operand,
        resume_bb: BasicBlockIndex,
        drop_bb: Option<BasicBlockIndex>,
    },
    GeneratorDrop,
}

impl Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Terminator {
    pub fn uses_vars_mut(&mut self) -> Vec<&mut LocalIndex> {
        match self {
            Terminator::GeneratorDrop
            | Terminator::DropAndReplace { .. }
            | Terminator::Drop { .. }
            | Terminator::Unreachable
            | Terminator::Goto { .. }
            | Terminator::Resume
            | Terminator::Abort => Vec::new(),
            Terminator::SwitchInt { discr, .. } => discr.uses_vars_mut(),
            Terminator::Return { local: ref mut v } => vec![v],
            Terminator::Call { func, .. } => func.uses_vars_mut(),
            Terminator::Assert { cond, .. } => cond.uses_vars_mut(),
            Terminator::Yield { value, .. } => value.uses_vars_mut(),
        }
    }
}

/// The top-level pack type.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub enum Pack {
    Mir(Mir),
}

impl Display for Pack {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let Pack::Mir(mir) = self;
        write!(f, "{}", mir)
    }
}

#[cfg(test)]
mod tests {
    use super::{Place, Rvalue, Statement};

    #[test]
    fn assign_uses_vars_mut() {
        let mut s = Statement::Assign(Place::Local(42), Rvalue::Place(Place::Local(43)));
        assert_eq!(s.uses_vars_mut(), vec![&mut 43]);
    }

    #[test]
    fn assign_defs_vars_mut() {
        let mut s = Statement::Assign(Place::Local(42), Rvalue::Place(Place::Local(43)));
        assert_eq!(s.defs_vars_mut(), vec![&mut 42]);
    }

    #[test]
    fn phi_uses_vars_mut() {
        let mut s = Statement::Assign(
            Place::Local(44),
            Rvalue::Phi(vec![Place::Local(100), Place::Local(200)]),
        );
        assert_eq!(s.uses_vars_mut(), vec![&mut 100, &mut 200]);
    }

    #[test]
    fn phi_defs_vars_mut() {
        let mut s = Statement::Assign(
            Place::Local(44),
            Rvalue::Phi(vec![Place::Local(100), Place::Local(200)]),
        );
        assert_eq!(s.defs_vars_mut(), vec![&mut 44]);
    }
}
