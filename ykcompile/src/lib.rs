//! The Yorick TIR trace compiler.

#![feature(proc_macro_hygiene)]
#![feature(test)]
#![feature(core_intrinsics)]

#[macro_use]
extern crate dynasmrt;
#[macro_use]
extern crate lazy_static;
extern crate test;

mod stack_builder;

use dynasmrt::{x64::Rq::*, Register};
use libc::{c_void, dlsym, RTLD_DEFAULT};
use stack_builder::StackBuilder;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::ffi::CString;
use std::fmt::{self, Display, Formatter};
use std::mem;
use std::process::Command;
use ykpack::{SignedIntTy, Ty, TypeId, UnsignedIntTy, IPlace};
use yktrace::tir::{
    BinOp, CallOperand, Constant, ConstantInt, Guard, Local, Operand, Place, Projection, Rvalue,
    Statement, TirOp, TirTrace,
};
use yktrace::{sir::SIR, INTERP_STEP_ARG};

use dynasmrt::{DynasmApi, DynasmLabelApi};

lazy_static! {
    // Registers that are caller-save as per the Sys-V ABI.
    static ref CALLER_SAVE_REGS: [u8; 8] = [RDI.code(), RSI.code(), RDX.code(), RCX.code(),
                                            R8.code(), R9.code(), R10.code(), R11.code()];

    // Register partitioning. These arrays must not overlap.
    // FIXME add callee save registers to the pool. Trace code will need to save/restore them.
    static ref TEMP_REG: u8 = R11.code();
    static ref TEMP_LOC: Location = Location::Register(*TEMP_REG);
    static ref LOCAL_REGS: [u8; 5] = [R10.code(), R9.code(), R8.code(), RDX.code(), RCX.code()];
}

//fn is_temp_reg(reg: u8) -> bool {
//    TEMP_REGS.contains(&reg)
//}

#[derive(Debug, Hash, Eq, PartialEq)]
pub enum CompileError {
    /// The binary symbol could not be found.
    UnknownSymbol(String),
}

impl Display for CompileError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSymbol(s) => write!(f, "Unknown symbol: {}", s),
        }
    }
}

/// Converts a register number into it's string name.
fn local_to_reg_name(loc: &Location) -> &'static str {
    match loc {
        Location::Register(r) => match r {
            0 => "rax",
            1 => "rcx",
            2 => "rdx",
            3 => "rbx",
            4 => "rsp",
            5 => "rbp",
            6 => "rsi",
            7 => "rdi",
            8 => "r8",
            9 => "r9",
            10 => "r10",
            11 => "r11",
            12 => "r12",
            13 => "r13",
            14 => "r14",
            15 => "r15",
            _ => unimplemented!(),
        },
        _ => "",
    }
}

/// A compiled `SIRTrace`.
pub struct CompiledTrace<TT> {
    /// A compiled trace.
    mc: dynasmrt::ExecutableBuffer,
    _pd: PhantomData<TT>,
}

impl<TT> CompiledTrace<TT> {
    /// Execute the trace by calling (not jumping to) the first instruction's address.
    pub fn execute(&self, args: &mut TT) {
        let func: fn(&mut TT) = unsafe { mem::transmute(self.mc.ptr(dynasmrt::AssemblyOffset(0))) };
        self.exec_trace(func, args);
    }

    /// Actually call the code. This is a separate function making it easier to set a debugger
    /// breakpoint right before entering the trace.
    fn exec_trace(&self, t_fn: fn(&mut TT), args: &mut TT) {
        t_fn(args);
    }
}

/// Represents a memory location using a register and an offset.
#[derive(Debug, Clone, PartialEq)]
pub struct RegAndOffset {
    reg: u8,
    offs: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Location {
    /// A value in a register.
    Register(u8),
    /// A statically known memory location relative to a register.
    Mem(RegAndOffset),
    // Location that contains a pointer to some underlying storage.
    //Addr(Box<Location>),
    /// A statically known constant.
    Const(Constant, TypeId),
    /// A non-live location. Used by the register allocator.
    NotLive,
}

impl Location {
    /// Creates a new memory location from a register and an offset.
    fn new_mem(reg: u8, offs: i32) -> Self {
        Self::Mem(RegAndOffset { reg, offs })
    }

    /// If `self` is a `Mem` then unwrap it, otherwise panic.
    fn unwrap_mem(&self) -> &RegAndOffset {
        if let Location::Mem(ro) = self {
            ro
        } else {
            panic!("tried to unwrap a Mem location when it wasn't a Mem");
        }
    }

    /// If `self` is a `Mem` then return a mutable reference to its innards, otherwise panic.
    fn unwrap_mem_mut(&mut self) -> &mut RegAndOffset {
        if let Location::Mem(ro) = self {
            ro
        } else {
            panic!("tried to unwrap a Mem location when it wasn't a Mem");
        }
    }

    /// If `self` is a `Register` then unwrap it, otherwise panic.
    fn unwrap_reg(&self) -> u8 {
        if let Location::Register(reg) = self {
            *reg
        } else {
            panic!("tried to unwrap a Register location when it wasn't a Register");
        }
    }
}

/// Allocation of one of the LOCAL_REGS. Temporary registers are tracked separately.
enum RegAlloc {
    Local(Local),
    Free,
}

use std::marker::PhantomData;
use ykpack::LocalDecl;

/// The `TraceCompiler` takes a `SIRTrace` and compiles it to machine code. Returns a `CompiledTrace`.
pub struct TraceCompiler<TT> {
    /// The dynasm assembler which will do all of the heavy lifting of the assembly.
    asm: dynasmrt::x64::Assembler,
    /// Stores the content of each register.
    register_content_map: HashMap<u8, RegAlloc>,
    /// Maps trace locals to their location (register, stack).
    variable_location_map: HashMap<Local, Location>,
    ///// Available temproary registers.
    //temp_regs: Vec<u8>,
    /// Local decls of the tir trace.
    local_decls: HashMap<Local, LocalDecl>,
    /// Stack builder for allocating objects on the stack.
    stack_builder: StackBuilder,
    addr_map: HashMap<String, u64>,
    _pd: PhantomData<TT>,
}

impl<TT> TraceCompiler<TT> {
    fn can_live_in_register(decl: &LocalDecl) -> bool {
        if decl.referenced {
            // We must allocate it on the stack so that we can reference it.
            return false;
        }

        // FIXME: optimisation: small structs and tuples etc. could actually live in a register.
        let ty = SIR.ty(&decl.ty);
        match ty {
            Ty::UnsignedInt(ui) => match ui {
                UnsignedIntTy::U128 => false,
                _ => true,
            },
            Ty::SignedInt(si) => match si {
                SignedIntTy::I128 => false,
                _ => true,
            },
            Ty::Array(_) => false,
            Ty::Slice(_) => false,
            Ty::Ref(_) | Ty::Bool => true,
            Ty::Struct(..) | Ty::Tuple(..) => false,
            Ty::Unimplemented(..) => todo!("{}", ty),
        }
    }

    /// Determine if the type needs to be copied when it is being dereferenced.
    fn is_copyable(tyid: &TypeId) -> bool {
        let ty = SIR.ty(tyid);
        match ty {
            Ty::UnsignedInt(ui) => match ui {
                UnsignedIntTy::U128 => false,
                _ => true,
            },
            Ty::SignedInt(si) => match si {
                SignedIntTy::I128 => false,
                _ => true,
            },
            // An array is copyable if its elements are.
            Ty::Array(ety) => Self::is_copyable(ety),
            Ty::Slice(ety) => Self::is_copyable(ety),
            Ty::Ref(_) | Ty::Bool => true,
            // FIXME A struct is copyable if it implements the Copy trait.
            Ty::Struct(..) => false,
            // FIXME A tuple is copyable if all its elements are.
            Ty::Tuple(..) => false,
            Ty::Unimplemented(..) => todo!("{}", ty),
        }
    }

    //fn place_to_location(&mut self, p: &Place, store: bool) -> (Location, Ty) {
    //    if !p.projection.is_empty() {
    //        self.resolve_projection(p, store)
    //    } else {
    //        let ty = self.place_ty(&Place::from(p.local)).clone();
    //        (self.local_to_location(p.local), ty)
    //    }
    //}

    fn iplace_to_location(&mut self, ip: &IPlace) -> Location {
        //if self.can_live_in_register(p.ty) {
        //} else {
        //}
        match ip {
            IPlace::Val{local, offs, ty} | IPlace::Deref{local, offs, ty} => {
                let mut loc = self.local_to_location(*local);

                // FIXME make a method on location.
                if *offs != 0 {
                    match &mut loc {
                        Location::Register(..) => {
                            // FIXME make it so that the "something" is allocated on the stack.
                            // Can we do this statically in the compiler?
                            todo!("offsetting something in a register");
                        },
                        Location::Mem(ro) => ro.offs += i32::try_from(*offs).unwrap(),
                        Location::Const(..) => todo!("offsetting a constant"),
                        Location::NotLive => unreachable!(),
                    }
                }
            },
            IPlace::Deref{local, offs, ty} => {
                let mut loc = self.local_to_location(*local);

                // FIXME make a method on location.
                if *offs != 0 {
                    match &mut loc {
                        Location::Register(..) => {
                            // FIXME make it so that the "something" is allocated on the stack.
                            // Can we do this statically in the compiler?
                            todo!("offsetting something in a register");
                        },
                        Location::Mem(ro) => ro.offs += i32::try_from(*offs).unwrap(),
                        Location::Const(..) => todo!("offsetting a constant"),
                        Location::NotLive => unreachable!(),
                    }
                }

                match 
                Location::Indirect
            },
            IPlace::Const{val, ty} => Location::Const(val.clone(), *ty),
            _ => todo!(),
        }
        //let base_loc = self.local_to_location
    }

    ///// Takes a `Place`, resolves all projections, and returns a `Location` containing the result.
    //fn resolve_projection(&mut self, p: &Place, store: bool) -> (Location, Ty) {
    //    let mut curloc = self.local_to_location(p.local);
    //    let mut ty = self.place_ty(&Place::from(p.local)).clone();
    //    let mut iter = p.projection.iter().peekable();
    //    while let Some(proj) = iter.next() {
    //        match proj {
    //            Projection::Field(idx) => match ty {
    //                Ty::Struct(ref sty) => {
    //                    let offs = sty.fields.offsets[usize::try_from(*idx).unwrap()];
    //                    let ftyid = &sty.fields.tys[usize::try_from(*idx).unwrap()];
    //                    curloc = self.resolve_field(curloc, ftyid, offs, store);
    //                    ty = SIR.ty(ftyid).clone();
    //                }
    //                Ty::Tuple(ref tty) => {
    //                    let offs = tty.fields.offsets[usize::try_from(*idx).unwrap()];
    //                    let ftyid = &tty.fields.tys[usize::try_from(*idx).unwrap()];
    //                    curloc = self.resolve_field(curloc, ftyid, offs, store);
    //                    ty = SIR.ty(ftyid).clone();
    //                }
    //                Ty::Ref(_) => unreachable!("ref"),
    //                _ => todo!("{:?}", ty),
    //            },
    //            Projection::Deref => {
    //                // FIXME We currently assume Deref is only called on Refs.

    //                // Are we dereferencing a reference, if so, what's its type.
    //                let tyid = match ty {
    //                    Ty::Ref(rty) => rty.clone(),
    //                    _ => todo!(),
    //                };

    //                // Special case: If the `Deref` is followed by an `Index` or `Field`
    //                // projection, we defer resolution to them and don't copy the value.
    //                // FIXME Do we need to check all remaining projections?
    //                let copy = match iter.peek() {
    //                    Some(Projection::Index(_)) => false,
    //                    Some(Projection::Field(_)) => false,
    //                    _ => true,
    //                };
    //                if Self::is_copyable(&tyid) && copy {
    //                    match SIR.ty(&tyid) {
    //                        Ty::Array(_) | Ty::Tuple(_) | Ty::Struct(_) => todo!(),
    //                        _ => {}
    //                    }
    //                    // Copy referenced value into a temporary.
    //                    let temp = self.create_temporary();
    //                    match &curloc {
    //                        Location::Mem(ro) => {
    //                            // Deref value and copy it.
    //                            dynasm!(self.asm
    //                                ; mov Rq(temp), [Rq(ro.reg) + ro.offs]
    //                            );
    //                        }
    //                        Location::Register(reg) | Location::Addr(reg) => {
    //                            dynasm!(self.asm
    //                                ; mov Rq(temp), Rq(reg)
    //                            );
    //                        }
    //                        _ => unreachable!(),
    //                    };
    //                    //self.free_if_temp(curloc);
    //                    curloc = if store {
    //                        Location::Addr(temp)
    //                    } else {
    //                        dynasm!(self.asm
    //                            ; mov Rq(temp), [Rq(temp)]
    //                        );
    //                        Location::Register(temp)
    //                    };
    //                    ty = SIR.ty(&tyid).clone();
    //                } else {
    //                    // Dereferencing a pointer, where the pointee is uncopyable, converts the
    //                    // location to an address.
    //                    let temp = self.create_temporary();
    //                    ty = SIR.ty(&tyid).clone(); // FIXME dedup
    //                    match &curloc {
    //                        Location::Mem(ro) => {
    //                            dynasm!(self.asm
    //                                ; mov Rq(temp), [Rq(ro.reg) + ro.offs]
    //                            );
    //                        }
    //                        Location::Register(reg) | Location::Addr(reg) => {
    //                            dynasm!(self.asm
    //                                ; mov Rq(temp), Rq(reg)
    //                            );
    //                        }
    //                        _ => unreachable!(),
    //                    }
    //                    //self.free_if_temp(curloc);
    //                    curloc = Location::Addr(temp);
    //                }
    //            }
    //            Projection::Index(local) => {
    //                // Get the type of the array elements.
    //                let elem_ty = match ty {
    //                    Ty::Array(ety) => SIR.ty(&ety),
    //                    Ty::Ref(tyid) => match SIR.ty(&tyid) {
    //                        Ty::Array(ety) => SIR.ty(&ety),
    //                        _ => unreachable!(),
    //                    },
    //                    _ => unreachable!(),
    //                };
    //                // Compute the offset of this index.
    //                let temp = self.create_temporary();
    //                match self.local_to_location(*local) {
    //                    Location::Register(reg) => {
    //                        dynasm!(self.asm
    //                            ; imul Rq(temp), Rq(reg), elem_ty.size() as i32
    //                        );
    //                    }
    //                    Location::Mem(ro) => {
    //                        dynasm!(self.asm
    //                            ; imul Rq(temp), [Rq(ro.reg) + ro.offs], elem_ty.size() as i32
    //                        );
    //                    }
    //                    _ => todo!(),
    //                }
    //                // Add together the index and the array address and retrieve its value.
    //                match &curloc {
    //                    Location::Mem(ro) => {
    //                        dynasm!(self.asm
    //                            ; add Rq(temp), [Rq(ro.reg) + ro.offs]
    //                        );
    //                    }
    //                    Location::Register(_) => todo!(),
    //                    Location::Addr(reg) => {
    //                        dynasm!(self.asm
    //                            ; add Rq(temp), Rq(reg)
    //                        );
    //                    }
    //                    _ => unreachable!(),
    //                }
    //                //self.free_if_temp(curloc);
    //                curloc = if store {
    //                    Location::Addr(temp)
    //                } else {
    //                    dynasm!(self.asm
    //                        ; mov Rq(temp), [Rq(temp)]
    //                    );
    //                    Location::Register(temp)
    //                };
    //                ty = elem_ty.clone();
    //            }
    //            _ => todo!("{}", p),
    //        }
    //    }
    //    (curloc, ty)
    //}

    //fn resolve_field(&mut self, loc: Location, tyid: &TypeId, offs: u64, store: bool) -> Location {
    //    // Convert Mem into Addr.
    //    let temp = self.create_temporary();
    //    match &loc {
    //        Location::Mem(ro) => {
    //            dynasm!(self.asm
    //                ; lea Rq(temp), [Rq(ro.reg) + ro.offs]
    //            );
    //        }
    //        Location::Register(reg) | Location::Addr(reg) => {
    //            dynasm!(self.asm
    //                ; mov Rq(temp), Rq(reg)
    //            );
    //        }
    //        _ => unreachable!("{:?}", loc),
    //    };
    //    //self.free_if_temp(loc);

    //    // Get index.
    //    dynasm!(self.asm
    //        ; lea Rq(temp), [Rq(temp) + i32::try_from(offs).unwrap()]
    //    );

    //    if store {
    //        return Location::Addr(temp);
    //    }
    //    if Self::can_live_in_register(tyid) {
    //        dynasm!(self.asm
    //            ; mov Rq(temp), [Rq(temp)]
    //        );
    //        Location::Register(temp)
    //    } else if Self::is_copyable(tyid) {
    //        todo!()
    //    } else {
    //        Location::Addr(temp)
    //    }
    //}

    /// Given a local, returns the register allocation for it, or, if there is no allocation yet,
    /// performs one.
    fn local_to_location(&mut self, l: Local) -> Location {
        if l == INTERP_STEP_ARG {
            // The argument is a mutable reference in RDI.
            Location::Mem(RegAndOffset{reg: RDI.code(), offs: 0})
        } else if let Some(location) = self.variable_location_map.get(&l) {
            // We already have a location for this local.
            location.clone()
        } else {
            let decl = &self.local_decls[&l];
            if Self::can_live_in_register(&decl) {
                // Find a free register to store this local.
                let loc = if let Some(reg) = self.get_free_register() {
                    self.register_content_map.insert(reg, RegAlloc::Local(l));
                    Location::Register(reg)
                } else {
                    // All registers are occupied, so we need to spill the local to the stack.
                    self.spill_local_to_stack(&l)
                };
                let ret = loc.clone();
                self.variable_location_map.insert(l, loc);
                ret
            } else {
                let ty = SIR.ty(&decl.ty);
                let loc = self.stack_builder.alloc(ty.size(), ty.align());
                self.variable_location_map.insert(l, loc.clone());
                loc
            }
        }
    }

    /// Returns a free register or `None` if all registers are occupied.
    fn get_free_register(&self) -> Option<u8> {
        self.register_content_map.iter().find_map(|(k, v)| match v {
            RegAlloc::Free => Some(*k),
            _ => None,
        })
    }

    /// Spill a local to the stack and return its location. Note: This does not update the
    /// `variable_location_map`.
    fn spill_local_to_stack(&mut self, local: &Local) -> Location {
        let tyid = self.local_decls[&local].ty;
        let ty = SIR.ty(&tyid);
        self.stack_builder.alloc(ty.size(), ty.align())
    }

    ///// Find a free register to be used as a temporary. If no free register can be found, a
    ///// register containing a Local is selected and its content spilled to the stack.
    //fn create_temporary(&mut self) -> u8 {
    //    self.temp_regs
    //        .pop()
    //        .unwrap_or_else(|| panic!("Exhausted temporary registers!"))
    //}

    /// Free the temporary register so it can be re-used.
    //fn free_if_temp(&mut self, loc: Location) {
    //    match loc {
    //        Location::Register(reg) | Location::Addr(reg) => {
    //            if is_temp_reg(reg) {
    //                debug_assert!(!self.temp_regs.contains(&reg), "double free temp reg");
    //                self.temp_regs.push(reg);
    //            }
    //        }
    //        Location::Mem { .. } => {}
    //        _ => unreachable!(),
    //    }
    //}

    /// Notifies the register allocator that the register allocated to `local` may now be re-used.
    fn free_register(&mut self, local: &Local) -> Result<(), CompileError> {
        match self.variable_location_map.get(local) {
            Some(Location::Register(reg)) => { //| Some(Location::Addr(reg)) => {
                //debug_assert!(!is_temp_reg(*reg));
                // If this local is currently stored in a register, free it.
                self.register_content_map.insert(*reg, RegAlloc::Free);
            }
            Some(Location::Mem { .. }) => {}
            Some(Location::NotLive) => unreachable!(),
            Some(Location::Const(..)) => unreachable!(),
            None => unreachable!("freeing unallocated register"),
        }
        self.variable_location_map.insert(*local, Location::NotLive);
        Ok(())
    }

    ///// Returns whether the register content map contains any temporaries. This is used as a sanity
    ///// check at the end of a trace to make sure we haven't forgotten to free temporaries at the
    ///// end of an operation.
    //fn check_temporaries(&self) -> bool {
    //    self.temp_regs.len() == TEMP_REGS.len()
    //}

    /// Copy bytes from one memory location to another.
    fn copy_memory(&mut self, dest: &RegAndOffset, src: &RegAndOffset, size: u64) {
        // We use memmove(3), as it's not clear if MIR (and therefore SIR) could cause copies
        // involving overlapping buffers.
        let sym = Self::find_symbol("memmove").unwrap();
        self.caller_save();
        dynasm!(self.asm
            ; push rax
            ; xor rax, rax
            ; lea rdi, [Rq(dest.reg) + dest.offs]
            ; lea rsi, [Rq(src.reg) + src.offs]
            ; mov rdx, size as i32
            ; mov r11, QWORD sym as i64
            ; call r11
            ; pop rax
        );
        self.caller_save_restore();
    }

    ///// Get the type of a place.
    //fn place_ty(&self, p: &Place) -> &Ty {
    //    SIR.ty(&self.local_decls[&p.local].ty)
    //}

    ///// Codegen a `Place` into a `Location`.
    //fn c_place(&mut self, p: &Place) -> Location {
    //    self.place_to_location(p, false).0
    //}

    ///// Codegen a reference into a `Location`.
    //fn c_ref(&mut self, p: &Place) -> Location {
    //    // Deal with the special case `&*`, i.e. referencing a `Deref` on a reference just returns
    //    // the reference.
    //    // FIXME Make sure the special case is only triggered for `&` on Refs and nothing else,
    //    // e.g. `&*`.
    //    if let Some(pj) = p.projection.get(0) {
    //        if matches!(pj, Projection::Deref)
    //            && matches!(SIR.ty(&self.local_decls[&p.local].ty), Ty::Ref(_))
    //        {
    //            // Clone the projection while removing the `Deref` from the end.
    //            let mut newproj = Vec::new();
    //            for p in p.projection.iter().take(p.projection.len() - 1) {
    //                newproj.push(p.clone());
    //            }
    //            let np = Place {
    //                local: p.local,
    //                projection: newproj,
    //            };
    //            let (rloc, _) = self.place_to_location(&np, false);
    //            let reg = self.create_temporary();
    //            match rloc {
    //                Location::Register(reg2) => {
    //                    dynasm!(self.asm
    //                        ; mov Rq(reg), Rq(reg2)
    //                    );
    //                }
    //                _ => todo!(),
    //            }
    //            //self.free_if_temp(rloc);
    //            return Location::Register(reg);
    //        }
    //    }

    //    // We can only reference Locals living on the stack. So move it there if it doesn't.
    //    let reg = self.create_temporary();
    //    let rloc = match self.place_to_location(p, false) {
    //        (Location::Register(reg2), _) => {
    //            let loc = self.stack_builder.alloc(8, 8);
    //            let ro = loc.unwrap_mem();
    //            dynasm!(self.asm
    //                ; mov [Rq(ro.reg) + ro.offs], Rq(reg2)
    //            );
    //            // This Local lives now on the stack...
    //            self.variable_location_map.insert(p.local, loc.clone());
    //            // ...so we can free its old register.
    //            //debug_assert!(!is_temp_reg(reg2));
    //            self.register_content_map.insert(reg2, RegAlloc::Free);
    //            loc
    //        }
    //        (loc, _) => loc,
    //    };
    //    // Now create the reference.
    //    match &rloc {
    //        Location::Mem(ro) => {
    //            dynasm!(self.asm
    //                ; lea Rq(reg), [Rq(ro.reg) + ro.offs]
    //            );
    //        }
    //        _ => unreachable!(),
    //    };
    //    //self.free_if_temp(rloc);
    //    Location::Register(reg)
    //}

    //fn c_len(&mut self, p: &Place) -> Location {
    //    let (loc, _) = self.place_to_location(p, true);
    //    let dst = self.create_temporary();
    //    match loc {
    //        Location::Addr(src) => {
    //            // A slice &[T] is a fat pointer with its length in the last 8 bytes.
    //            dynasm!(self.asm
    //                ; mov Rq(dst), [Rq(src) + 8]
    //            );
    //        }
    //        // FIXME Can `Len` be called on non-references?
    //        _ => unreachable!(),
    //    }
    //    //self.free_if_temp(loc);
    //    Location::Register(dst)
    //}

    /// Emit a NOP operation.
    fn nop(&mut self) {
        dynasm!(self.asm
            ; nop
        );
    }

    /// Codegen a constant integer into a `Location`.
    //fn c_constint(&mut self, constant: &ConstantInt) -> Location {
    //    let reg = self.create_temporary();
    //    let c_val = constant.i64_cast();
    //    dynasm!(self.asm
    //        ; mov Rq(reg), QWORD c_val
    //    );
    //    Location::Register(reg)
    //}

    ///// Codegen a Boolean into a `Location`.
    //fn c_bool(&mut self, b: bool) -> Location {
    //    let reg = self.create_temporary();
    //    dynasm!(self.asm
    //        ; mov Rq(reg), QWORD b as i64
    //    );
    //    Location::Register(reg)
    //}

    ///// Compile the entry into an inlined function call.
    //fn c_enter(&mut self, args: &Vec<Operand>, off: u32) {
    //    // Move call arguments into registers.
    //    for (op, i) in args.iter().zip(1..) {
    //        let loc = match op {
    //            Operand::Place(p) => self.c_place(p),
    //            Operand::Constant(c) => match c {
    //                Constant::Int(ci) => self.c_constint(ci),
    //                Constant::Bool(b) => self.c_bool(*b),
    //                c => todo!("{}", c),
    //            },
    //        };
    //        let arg_idx = Place::from(Local(i + off));
    //        self.store(&arg_idx, loc.clone());
    //        //self.free_if_temp(loc);
    //    }
    //}

    /// Push all of the caller-save registers to the stack.
    fn caller_save(&mut self) {
        for reg in CALLER_SAVE_REGS.iter() {
            dynasm!(self.asm
                ; push Rq(reg)
            );
        }
    }

    /// Restore caller-save registers from the stack.
    fn caller_save_restore(&mut self) {
        for reg in CALLER_SAVE_REGS.iter().rev() {
            dynasm!(self.asm
                ; pop Rq(reg)
            );
        }
    }

    ///// Compile a call to a native symbol using the Sys-V ABI. This is used for occasions where you
    ///// don't want to, or cannot, inline the callee (e.g. it's a foreign function).
    /////
    ///// For now we do something very simple. There are limitations (FIXME):
    /////
    /////  - We assume there are no more than 6 arguments (spilling is not yet implemented).
    /////
    /////  - We push all of the callee save registers on the stack, and local variable arguments are
    /////    then loaded back from the stack into the correct ABI-specified registers. We can
    /////    optimise this later by only loading an argument from the stack if it cannot be loaded
    /////    from its original register location (because another argument overwrote it already).
    /////
    /////  - We assume the return value fits in rax. 128-bit return values are not yet supported.
    /////
    /////  - We don't support varags calls.
    /////
    /////  - RAX is clobbered.
    //fn c_call(
    //    &mut self,
    //    opnd: &CallOperand,
    //    args: &Vec<Operand>,
    //    dest: &Option<Place>,
    //) -> Result<(), CompileError> {
    //    let sym = if let CallOperand::Fn(sym) = opnd {
    //        sym
    //    } else {
    //        todo!("unknown call target");
    //    };

    //    if args.len() > 6 {
    //        todo!("call with spilled args");
    //    }

    //    // Save Sys-V caller save registers to the stack, but skip the one (if there is one) that
    //    // will store the return value. It's safe to assume the caller expects this to be
    //    // clobbered.
    //    //
    //    // FIXME: Note that we don't save RAX. Although this is a caller save register, we are
    //    // currently using RAX as a general purpose register in parts of the compiler (the register
    //    // allocator thus never gives out RAX). In this case we use it to store the result from the
    //    // call in its destination, so we must not override it when returning from the call.
    //    self.caller_save();

    //    // Helper function to find the index of a caller-save register previously pushed to the stack.
    //    // The first register pushed is at the highest stack offset (from the stack pointer), hence
    //    // reversing the order of `save_regs`.
    //    let stack_index = |reg: u8| -> i32 {
    //        i32::try_from(
    //            CALLER_SAVE_REGS
    //                .iter()
    //                .rev()
    //                .position(|&r| r == reg)
    //                .unwrap(),
    //        )
    //        .unwrap()
    //    };

    //    // Sys-V ABI dictates the first 6 arguments are passed in these registers.
    //    // The order is reversed so they pop() in the right order.
    //    let mut arg_regs = vec![R9, R8, RCX, RDX, RSI, RDI]
    //        .iter()
    //        .map(|r| r.code())
    //        .collect::<Vec<u8>>();

    //    for arg in args {
    //        // `unwrap()` must succeed, as we checked there are no more than 6 args above.
    //        let arg_reg = arg_regs.pop().unwrap();

    //        match arg {
    //            Operand::Place(place) => {
    //                // Load argument back from the stack.
    //                let (loc, _) = self.place_to_location(place, false);
    //                match &loc {
    //                    Location::Register(reg) => {
    //                        let off = stack_index(*reg) * 8;
    //                        dynasm!(self.asm
    //                            ; mov Rq(arg_reg), [rsp + off]
    //                        );
    //                    }
    //                    Location::Mem(ro) => {
    //                        dynasm!(self.asm
    //                            ; mov Rq(arg_reg), [Rq(ro.reg) + ro.offs]
    //                        );
    //                    }
    //                    Location::Addr(_) | Location::NotLive => unreachable!(),
    //                };
    //                //self.free_if_temp(loc);
    //            }
    //            Operand::Constant(c) => {
    //                dynasm!(self.asm
    //                    ; mov Rq(arg_reg), QWORD c.i64_cast()
    //                );
    //            }
    //        };
    //    }

    //    let sym_addr = if let Some(addr) = self.addr_map.get(sym) {
    //        *addr as i64
    //    } else {
    //        TraceCompiler::<TT>::find_symbol(sym)? as i64
    //    };
    //    dynasm!(self.asm
    //        // In Sys-V ABI, `al` is a hidden argument used to specify the number of vector args
    //        // for a vararg call. We don't support this right now, so set it to zero.
    //        ; xor rax, rax
    //        ; mov r11, QWORD sym_addr
    //        ; call r11
    //    );

    //    // Restore caller-save registers.
    //    self.caller_save_restore();

    //    if let Some(d) = dest {
    //        self.store(d, Location::Register(RAX.code()));
    //    }

    //    Ok(())
    //}

    /// Compile a checked binary operation into a `Location`.
    //fn c_checked_binop(&mut self, binop: &BinOp, op1: &Operand, op2: &Operand) -> Location {
    //    // Move `op1` into `dest`.
    //    let dest_loc = match op1 {
    //        Operand::Place(p) => match self.place_to_location(p, false) {
    //            (Location::Mem(ro), ty) => {
    //                let tmp = self.create_temporary();
    //                match ty.size() {
    //                    1 => {
    //                        dynasm!(self.asm
    //                            ; mov Rb(tmp), BYTE [Rq(ro.reg) + ro.offs]
    //                        );
    //                    }
    //                    2 => {
    //                        dynasm!(self.asm
    //                            ; mov Rw(tmp), WORD [Rq(ro.reg) + ro.offs]
    //                        );
    //                    }
    //                    4 => {
    //                        dynasm!(self.asm
    //                            ; mov Rd(tmp), DWORD [Rq(ro.reg) + ro.offs]
    //                        );
    //                    }
    //                    8 => {
    //                        dynasm!(self.asm
    //                            ; mov Rq(tmp), QWORD [Rq(ro.reg) + ro.offs]
    //                        );
    //                    }
    //                    _ => unreachable!(),
    //                }
    //                Location::Register(tmp)
    //            }
    //            (other, _) => other,
    //        },
    //        Operand::Constant(Constant::Int(ci)) => self.c_constint(&ci),
    //        Operand::Constant(Constant::Bool(_b)) => unreachable!(),
    //        Operand::Constant(c) => todo!("{}", c),
    //    };
    //    let dest = dest_loc.unwrap_reg();
    //    // Add together `dest` and `op2`.
    //    match op2 {
    //        Operand::Place(p) => {
    //            let (rloc, _) = self.place_to_location(&p, false);
    //            match binop {
    //                BinOp::Add => self.c_checked_add_place(dest, &rloc),
    //                _ => todo!(),
    //            }
    //            //self.free_if_temp(rloc);
    //        }
    //        Operand::Constant(Constant::Int(ci)) => match binop {
    //            BinOp::Add => self.c_checked_add_const(dest, ci),
    //            _ => todo!(),
    //        },
    //        Operand::Constant(Constant::Bool(_b)) => todo!(),
    //        Operand::Constant(c) => todo!("{}", c),
    //    };
    //    // In the future this will set the overflow flag of the tuple in `lloc`, which will be
    //    // checked by a guard, allowing us to return from the trace more gracefully.
    //    dynasm!(self.asm
    //        ; jc ->crash
    //    );
    //    dest_loc
    //}

    // FIXME Use a macro to generate funcs for all of the different binary operations.
    // Code-gen the addition of a `Location` to the value in the register `dest_reg`.
    fn c_checked_add_place(&mut self, dest_reg: u8, src_loc: &Location) {
        match src_loc {
            Location::Register(reg) => {
                dynasm!(self.asm
                    ; add Rq(dest_reg), Rq(reg)
                );
            }
            Location::Mem(ro) => {
                dynasm!(self.asm
                    ; add Rq(dest_reg), [Rq(ro.reg) + ro.offs]
                );
            }
            _ => unreachable!(),
        }
    }

    // Code-gen the addition of a constant integer to the value in the register `dest_reg`.
    fn c_checked_add_const(&mut self, dest_reg: u8, src_const: &ConstantInt) {
        let c_val = src_const.i64_cast();
        if c_val <= u32::MAX.into() {
            dynasm!(self.asm
                ; add Rq(dest_reg), c_val as u32 as i32
            );
        } else {
            dynasm!(self.asm
                ; mov rax, QWORD c_val
                ; add Rq(dest_reg), rax
            );
        }
    }

    /// Load an IPlace into the temporary register. Panic if it doesn't fit.
    fn load_temp_reg(&mut self, ip: &IPlace) {
        let loc = self.iplace_to_location(ip);
        match loc {
            Location::Register(r) => {
                dynasm!(self.asm
                    ; mov Rq(*TEMP_REG), Rq(r)
                );
            },
            Location::Mem(ro) => {
                match SIR.ty(&ip.ty()).size() {
                    1 | 2 | 4 => todo!(),
                    8 => {
                        dynasm!(self.asm
                            ; mov Rb(*TEMP_REG), BYTE [Rq(ro.reg) + ro.offs]
                        );
                    }
                    _ => unreachable!("doesn't fit"),
                }
            },
            Location::Const(..) => todo!(), // FIXME pull code from store() and put in a func?
            Location::NotLive => unreachable!(),
        }
    }

    fn c_binop(&mut self, dest: &IPlace, op: BinOp, opnd1: &IPlace, opnd2: &IPlace, checked: bool) {
        dbg!(opnd1, opnd2);

        // FIXME result not yet checked.
        if op != BinOp::Add {
            todo!();
        }

        // We do this in three stages.
        // 1) Copy the first operand into the temp register.
        self.load_temp_reg(opnd1);

        // 2) Add the second operand.
        let src_loc = self.iplace_to_location(opnd2);
        let size = SIR.ty(&opnd1.ty()).size();
        match src_loc {
            Location::Register(r) => {
                match size {
                    1 | 2 | 4 => todo!(),
                    8 => {
                        dynasm!(self.asm
                            ; add Rq(*TEMP_REG), Rq(r)
                        );
                    },
                    _ => unreachable!(format!("{}", SIR.ty(&dest.ty()))),
                }
            },
            Location::Mem(..) => todo!(),
            Location::Const(..) => todo!(),
            Location::NotLive => todo!(),
        }

        // 3) Move the result to where it is supposed to live.
        // If it is a checked operation, then we have to build a (value, overflow?) tuple.
        let mut dest_loc = self.iplace_to_location(dest);
        self.store_raw(&dest_loc, &*TEMP_LOC, size);
        if checked {
            // Set overflow flag.
            // FIXME assumes it doesn't overflow for now.
            dynasm!(self.asm
                ; mov Rq(*TEMP_REG), 0
            );
            let ro = dest_loc.unwrap_mem_mut();
            ro.offs += i32::try_from(size).unwrap();
            self.store_raw(&dest_loc, &*TEMP_LOC, 1);
        }
    }

    /// Compile a TIR statement.
    fn c_statement(&mut self, stmt: &Statement) -> Result<(), CompileError> {
        match stmt {
            Statement::IStore(dest, src) => self.c_istore(dest, src),
            Statement::BinaryOp{dest, op, opnd1, opnd2, checked} => self.c_binop(dest, *op, opnd1, opnd2, *checked),
            Statement::MkRef(dest, src) => self.c_mkref(dest, src),
            //Statement::Deref(dest, src) => todo!(), //self.c_deref(dest, src),
            Statement::Enter(_, args, _dest, off) => todo!(), //self.c_enter(args, *off),
            Statement::Leave => {}
            Statement::StorageDead(l) => self.free_register(l)?,
            Statement::Call(target, args, dest) => todo!(), //self.c_call(target, args, dest)?,
            Statement::Nop => {}
            Statement::Unimplemented(s) => todo!("{:?}", s),
            Statement::Debug(..) => {},
        }

        Ok(())
    }

    fn c_mkref(&mut self, dest: &IPlace, src: &IPlace) {
        let src_loc = self.iplace_to_location(src);
        match src_loc {
            Location::Register(..) => {
                // This isn't possible as the allocator explicitely puts things which are
                // referenced onto the stack and never in registers.
                unreachable!()
            },
            Location::Mem(ro) => {
                dynasm!(self.asm
                    ; lea Rq(*TEMP_REG), [Rq(ro.reg) + ro.offs]
                );
            },
            Location::Const(..) => todo!(),
            Location::NotLive => unreachable!(),
        }
        let dest_loc = self.iplace_to_location(src);
        self.store_raw(&dest_loc, &*TEMP_LOC, SIR.ty(&src.ty()).size());
    }

    fn c_istore(&mut self, dest: &IPlace, src: &IPlace) {
        self.store(dest, src);
    }

    /// Store the value in `src_loc` into `dest_loc`.
    fn store(&mut self, dest_ip: &IPlace, src_ip: &IPlace) {
        let dest_loc = self.iplace_to_location(dest_ip);
        let src_loc = self.iplace_to_location(src_ip);
        self.store_raw(&dest_loc, &src_loc, SIR.ty(&dest_ip.ty()).size());
    }

    /// Stores src_loc into dest_loc.
    fn store_raw(&mut self, dest_loc: &Location, src_loc: &Location, size: u64) {
        match (&dest_loc, &src_loc) {
            // (Location::Addr(dest_reg), Location::Register(src_reg)) => {
            //     // If the lhs is a projection that results in a memory address (e.g.
            //     // `(*$1).0`), then the value in `dest_reg` is a pointer to store into.
            //     match ty.size() {
            //         0 => (), // ZST.
            //         1 => {
            //             dynasm!(self.asm
            //                 ; mov [Rq(dest_reg)], Rb(src_reg)
            //             );
            //         }
            //         2 => {
            //             dynasm!(self.asm
            //                 ; mov [Rq(dest_reg)], Rw(src_reg)
            //             );
            //         }
            //         4 => {
            //             dynasm!(self.asm
            //                 ; mov [Rq(dest_reg)], Rd(src_reg)
            //             );
            //         }
            //         8 => {
            //             dynasm!(self.asm
            //                 ; mov [Rq(dest_reg)], Rq(src_reg)
            //             );
            //         }
            //         _ => unreachable!(),
            //     }
            // }
            (Location::Register(dest_reg), Location::Register(src_reg)) => {
                dynasm!(self.asm
                    ; mov Rq(dest_reg), Rq(src_reg)
                );
            }
            (Location::Mem(dest_ro), Location::Register(src_reg)) => {
                match size {
                    0 => (), // ZST.
                    1 => {
                        dynasm!(self.asm
                            ; mov BYTE [Rq(dest_ro.reg) + dest_ro.offs], Rb(src_reg)
                        );
                    }
                    2 => {
                        dynasm!(self.asm
                            ; mov WORD [Rq(dest_ro.reg) + dest_ro.offs], Rw(src_reg)
                        );
                    }
                    4 => {
                        dynasm!(self.asm
                            ; mov DWORD [Rq(dest_ro.reg) + dest_ro.offs], Rd(src_reg)
                        );
                    }
                    8 => {
                        dynasm!(self.asm
                            ; mov QWORD [Rq(dest_ro.reg) + dest_ro.offs], Rq(src_reg)
                        );
                    }
                    _ => unreachable!(),
                }
            }
            (Location::Mem(dest_ro), Location::Mem(src_ro)) => {
                if size <= 8 {
                    match size {
                        0 => (), // ZST.
                        1 => {
                            dynasm!(self.asm
                                ; mov Rb(*TEMP_REG), BYTE [Rq(src_ro.reg) + src_ro.offs]
                                ; mov BYTE [Rq(dest_ro.reg) + dest_ro.offs], Rb(*TEMP_REG)
                            );
                        }
                        2 => {
                            dynasm!(self.asm
                                ; mov Rw(*TEMP_REG), WORD [Rq(src_ro.reg) + src_ro.offs]
                                ; mov WORD [Rq(dest_ro.reg) + dest_ro.offs], Rw(*TEMP_REG)
                            );
                        }
                        4 => {
                            dynasm!(self.asm
                                ; mov Rd(*TEMP_REG), DWORD [Rq(src_ro.reg) + src_ro.offs]
                                ; mov DWORD [Rq(dest_ro.reg) + dest_ro.offs], Rd(*TEMP_REG)
                            );
                        }
                        8 => {
                            dynasm!(self.asm
                                ; mov Rq(*TEMP_REG), QWORD [Rq(src_ro.reg) + src_ro.offs]
                                ; mov QWORD [Rq(dest_ro.reg) + dest_ro.offs], Rq(*TEMP_REG)
                            );
                        }
                        _ => unreachable!(),
                    }
                    //self.free_if_temp(Location::Register(temp));
                } else {
                    self.copy_memory(dest_ro, src_ro, size);
                }
            }
            //(Location::Register(dest_reg, dest_is_ptr), Location::Mem(src_ro)) => {
            // (Location::Addr(dest_reg), Location::Mem(src_ro)) => {
            //     if ty.size() <= 8 {
            //         let temp = self.create_temporary();
            //         match ty.size() {
            //             0 => (), // ZST.
            //             1 => {
            //                 dynasm!(self.asm
            //                     ; mov Rb(temp), BYTE [Rq(src_ro.reg) + src_ro.offs]
            //                     ; mov BYTE [Rq(dest_reg)], Rb(temp)
            //                 );
            //             }
            //             2 => {
            //                 dynasm!(self.asm
            //                     ; mov Rw(temp), WORD [Rq(src_ro.reg) + src_ro.offs]
            //                     ; mov WORD [Rq(dest_reg)], Rw(temp)
            //                 );
            //             }
            //             4 => {
            //                 dynasm!(self.asm
            //                     ; mov Rd(temp), DWORD [Rq(src_ro.reg) + src_ro.offs]
            //                     ; mov DWORD [Rq(dest_reg)], Rd(temp)
            //                 );
            //             }
            //             8 => {
            //                 dynasm!(self.asm
            //                     ; mov Rq(temp), QWORD [Rq(src_ro.reg) + src_ro.offs]
            //                     ; mov QWORD [Rq(dest_reg)], Rq(temp)
            //                 );
            //             }
            //             _ => unreachable!(),
            //         }
            //         //self.free_if_temp(Location::Register(temp));
            //     } else {
            //         self.copy_memory(
            //             &RegAndOffset {
            //                 reg: *dest_reg,
            //                 offs: 0,
            //             },
            //             src_ro,
            //             ty.size(),
            //         );
            //     }
            // }
            (Location::Register(dest_reg), Location::Mem(src_ro)) => {
                match size {
                    0 => (), // ZST.
                    1 => {
                        dynasm!(self.asm
                            ; mov Rb(dest_reg), BYTE [Rq(src_ro.reg) + src_ro.offs]
                        );
                    }
                    2 => {
                        dynasm!(self.asm
                            ; mov Rw(dest_reg), WORD [Rq(src_ro.reg) + src_ro.offs]
                        );
                    }
                    4 => {
                        dynasm!(self.asm
                            ; mov Rd(dest_reg), DWORD [Rq(src_ro.reg) + src_ro.offs]
                        );
                    }
                    8 => {
                        dynasm!(self.asm
                            ; mov Rq(dest_reg), QWORD [Rq(src_ro.reg) + src_ro.offs]
                        );
                    }
                    _ => unreachable!(),
                }
            }
            (Location::Register(dest_reg), Location::Const(c_val, _)) => {
                let i64_c = c_val.i64_cast();
                if size > 0 {
                    if i64_c < i32::MAX as i64 {
                        let i32_c = i32::try_from(c_val.i64_cast()).unwrap();
                        dynasm!(self.asm
                            ; mov Rq(dest_reg), i64_c as i32
                        );
                    } else {
                        // Can't move 64-bit constants in x86_64.
                        let i64_c = c_val.i64_cast();
                        let hi_word = (i64_c >> 32) as i32;
                        let lo_word = (i64_c & 0xffffffff) as i32;
                        dynasm!(self.asm
                            ; mov Rq(dest_reg), hi_word
                            ; shl Rq(dest_reg), 32
                            ; or Rq(dest_reg), lo_word
                        );
                    }
                }
            },
            (Location::Mem(ro), Location::Const(c_val, ty)) => {
                // FIXME this assumes the constant fits in 64 bits. We could have things like
                // large constant tuples or u128 even.
                let c_i64 = c_val.i64_cast();
                match SIR.ty(&ty).size() {
                    1 => {
                        dynasm!(self.asm
                            ; mov BYTE [Rq(ro.reg) + ro.offs], c_i64 as i8
                        );
                    },
                    2 => {
                        dynasm!(self.asm
                            ; mov WORD [Rq(ro.reg) + ro.offs], c_i64 as i16
                        );
                    },
                    4 => {
                        dynasm!(self.asm
                            ; mov DWORD [Rq(ro.reg) + ro.offs], c_i64 as i32
                        );
                    },
                    8 => {
                        let hi = c_i64 >> 32;
                        let lo = c_i64 & 0xffffffff;
                        dynasm!(self.asm
                            ; mov QWORD [Rq(ro.reg) + ro.offs], lo as i32
                            ; mov QWORD [Rq(ro.reg) + ro.offs + 4], hi as i32
                        );
                    },
                    _ => todo!(),
                }
            }
            _ => unreachable!(),
        }
        //self.free_if_temp(dest_loc);
    }

    /// Compile a guard in the trace, emitting code to abort execution in case the guard fails.
    fn c_guard(&mut self, _grd: &Guard) {
        self.nop(); // FIXME compile guards
    }

    /// Print information about the state of the compiler and exit.
    fn crash_dump(self, e: Option<CompileError>) -> ! {
        eprintln!("\nThe trace compiler crashed!\n");

        if let Some(e) = e {
            eprintln!("Reason: {}.\n", e);
        } else {
            eprintln!("Reason: unknown");
        }

        // To help us figure out what has gone wrong, we can print the disassembled instruction
        // stream with the help of `rasm2`.
        eprintln!("Executable code buffer:");
        let code = &*self.asm.finalize().unwrap();
        if code.is_empty() {
            eprintln!("  <empty buffer>");
        } else {
            let hex_code = hex::encode(code);
            let res = Command::new("rasm2")
                .arg("-d")
                .arg("-b 64") // x86_64.
                .arg(hex_code.clone())
                .output()
                .unwrap();
            if !res.status.success() {
                eprintln!("  Failed to invoke rasm2. Raw bytes follow...");
                eprintln!("  {}", hex_code);
            } else {
                let asm = String::from_utf8(res.stdout).unwrap();
                for line in asm.lines() {
                    eprintln!("  {}", line);
                }
            }
        }

        // Print the register allocation.
        eprintln!("\nRegister allocation (place -> reg):");
        for (place, location) in &self.variable_location_map {
            eprintln!(
                "  {:2} -> {:?} ({})",
                place,
                location,
                local_to_reg_name(location)
            );
        }
        eprintln!();

        panic!("stopped due to trace compilation error");
    }

    /// Emit a return instruction.
    fn ret(&mut self) {
        // Reset the stack/base pointers and return from the trace. We also need to generate the
        // code that reserves stack space for spilled locals here, since we don't know at the
        // beginning of the trace how many locals are going to be spilled.
        let soff = self.stack_builder.size();
        dynasm!(self.asm
            ; add rsp, soff as i32
            ; pop rbp
            ; ret
            ; ->reserve:
            ; push rbp
            ; mov rbp, rsp
            ; sub rsp, soff as i32
            ; jmp ->main
        );
    }

    fn init(&mut self) {
        // Jump to the label that reserves stack space for spilled locals.
        dynasm!(self.asm
            ; jmp ->reserve
            ; ->crash:
            ; ud2
            ; ->main:
        );
    }

    /// Finish compilation and return the executable code that was assembled.
    fn finish(self) -> dynasmrt::ExecutableBuffer {
        self.asm.finalize().unwrap()
    }

    #[cfg(test)]
    fn test_compile(tt: TirTrace) -> (CompiledTrace<TT>, u32) {
        // Changing the registers available to the register allocator affects the number of spills,
        // and thus also some tests. To make sure we notice when this happens we also check the
        // number of spills in those tests. We thus need a slightly different version of the
        // `compile` function that provides this information to the test.
        let tc = TraceCompiler::<TT>::_compile(tt);
        let spills = tc.stack_builder.size();
        let ct = CompiledTrace::<TT> {
            mc: tc.finish(),
            _pd: PhantomData,
        };
        (ct, spills)
    }

    /// Compile a TIR trace, returning executable code.
    pub fn compile(tt: TirTrace) -> CompiledTrace<TT> {
        let tc = TraceCompiler::<TT>::_compile(tt);
        CompiledTrace::<TT> {
            mc: tc.finish(),
            _pd: PhantomData,
        }
    }

    fn _compile(tt: TirTrace) -> Self {
        let assembler = dynasmrt::x64::Assembler::new().unwrap();

        // Make the TirTrace mutable so we can drain it into the TraceCompiler.
        let mut tt = tt;
        let mut tc = TraceCompiler::<TT> {
            asm: assembler,
            //temp_regs: Vec::from(*TEMP_REGS),
            register_content_map: LOCAL_REGS.iter().map(|r| (*r, RegAlloc::Free)).collect(),
            variable_location_map: HashMap::new(),
            local_decls: tt.local_decls.clone(),
            stack_builder: StackBuilder::default(),
            addr_map: tt.addr_map.drain().into_iter().collect(),
            _pd: PhantomData,
        };

        tc.init();

        for i in 0..tt.len() {
            let res = match unsafe { tt.op(i) } {
                TirOp::Statement(st) => tc.c_statement(st),
                TirOp::Guard(g) => Ok(tc.c_guard(g)),
            };

            // FIXME -- Later errors should not be fatal. We should be able to abort trace
            // compilation and carry on.
            match res {
                Ok(_) => (),
                Err(e) => tc.crash_dump(Some(e)),
            }
        }

        // Make sure we didn't forget to free some temporaries.
        //assert!(tc.check_temporaries());
        tc.ret();
        tc
    }

    /// Returns a pointer to the static symbol `sym`, or an error if it cannot be found.
    fn find_symbol(sym: &str) -> Result<*mut c_void, CompileError> {
        let sym_arg = CString::new(sym).unwrap();
        let addr = unsafe { dlsym(RTLD_DEFAULT, sym_arg.into_raw()) };

        if addr == 0 as *mut c_void {
            Err(CompileError::UnknownSymbol(sym.to_owned()))
        } else {
            Ok(addr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompileError, HashMap, Local, Location, RegAlloc, TraceCompiler, LOCAL_REGS};
    use crate::stack_builder::StackBuilder;
    use fm::FMBuilder;
    use libc::{abs, c_void, getuid};
    use regex::Regex;
    use std::marker::PhantomData;
    use yktrace::sir::SIR;
    use yktrace::tir::TirTrace;
    use yktrace::{start_tracing, TracingKind};

    extern "C" {
        fn add6(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64;
    }
    extern "C" {
        fn add_some(a: u64, b: u64, c: u64, d: u64, e: u64) -> u64;
    }

    /// Fuzzy matches the textual TIR for the trace `tt` with the pattern `ptn`.
    fn assert_tir(ptn: &str, tt: &TirTrace) {
        let ptn_re = Regex::new(r"%.+?\b").unwrap(); // Names are words prefixed with `%`.
        let text_re = Regex::new(r"\$?.+?\b").unwrap(); // Any word optionally prefixed with `$`.
        let matcher = FMBuilder::new(ptn)
            .unwrap()
            .name_matcher(Some((ptn_re, text_re)))
            .distinct_name_matching(true)
            .build()
            .unwrap();

        let res = matcher.matches(&format!("{}", tt));
        if let Err(e) = res {
            eprintln!("{}", e); // Visible when tests run with --nocapture.
            panic!(e);
        }
    }

    #[test]
    fn test_simple() {
        struct IO(u8);

        #[interp_step]
        #[inline(never)]
        fn simple(io: &mut IO) {
            let x = 13;
            io.0 = x;
        }

        let th = start_tracing(TracingKind::HardwareTracing);
        simple(&mut IO(0));
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        println!("{}", tir_trace);
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 13);
    }

    // Repeatedly fetching the register for the same local should yield the same register and
    // should not exhaust the allocator.
    #[ignore] // Broken because we don't know what type IDs to put in local_decls.
    #[test]
    fn reg_alloc_same_local() {
        struct IO(u8);
        let mut tc = TraceCompiler::<IO> {
            asm: dynasmrt::x64::Assembler::new().unwrap(),
            register_content_map: LOCAL_REGS
                .iter()
                .cloned()
                .map(|r| (r, RegAlloc::Free))
                .collect(),
            //temp_regs: Vec::from(*TEMP_REGS),
            variable_location_map: HashMap::new(),
            local_decls: HashMap::default(),
            stack_builder: StackBuilder::default(),
            addr_map: HashMap::new(),
            _pd: PhantomData,
        };

        for _ in 0..32 {
            assert_eq!(
                tc.local_to_location(Local(1)),
                tc.local_to_location(Local(1))
            );
        }
    }

    // Locals should be allocated to different registers.
    #[ignore] // Broken because we don't know what type IDs to put in local_decls.
    #[test]
    fn reg_alloc() {
        let local_decls = HashMap::new();
        struct IO(u8);
        let mut tc = TraceCompiler::<IO> {
            asm: dynasmrt::x64::Assembler::new().unwrap(),
            register_content_map: LOCAL_REGS
                .iter()
                .cloned()
                .map(|r| (r, RegAlloc::Free))
                .collect(),
            //temp_regs: Vec::from(*TEMP_REGS),
            variable_location_map: HashMap::new(),
            local_decls,
            stack_builder: StackBuilder::default(),
            addr_map: HashMap::new(),
            _pd: PhantomData,
        };

        let mut seen: Vec<Location> = Vec::new();
        for l in 0..7 {
            let reg = tc.local_to_location(Local(l));
            assert!(!seen.contains(&reg));
            seen.push(reg);
        }
    }

    #[inline(never)]
    fn farg(i: u8) -> u8 {
        i
    }

    #[test]
    fn test_function_call_simple() {
        struct IO(u8);

        #[interp_step]
        #[inline(never)]
        fn fcall(io: &mut IO) {
            io.0 = farg(13);
            let _z = farg(14);
        }

        let mut io = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        fcall(&mut io);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 13);
    }

    #[test]
    fn test_function_call_nested() {
        struct IO(u8);

        fn fnested3(i: u8, _j: u8) -> u8 {
            let c = i;
            c
        }

        fn fnested2(i: u8) -> u8 {
            fnested3(i, 10)
        }

        #[interp_step]
        fn fnested(io: &mut IO) {
            io.0 = fnested2(20);
        }

        let mut io = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        fnested(&mut io);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 20);
    }

    // Test finding a symbol in a shared object.
    #[test]
    fn find_symbol_shared() {
        struct IO(());
        assert!(TraceCompiler::<IO>::find_symbol("printf") == Ok(libc::printf as *mut c_void));
    }

    // Test finding a symbol in the main binary.
    // For this to work the binary must have been linked with `--export-dynamic`, which ykrustc
    // appends to the linker command line.
    #[test]
    #[no_mangle]
    fn find_symbol_main() {
        struct IO(());
        assert!(
            TraceCompiler::<IO>::find_symbol("find_symbol_main")
                == Ok(find_symbol_main as *mut c_void)
        );
    }

    // Check that a non-existent symbol cannot be found.
    #[test]
    fn find_nonexistent_symbol() {
        struct IO(());
        assert_eq!(
            TraceCompiler::<IO>::find_symbol("__xxxyyyzzz__"),
            Err(CompileError::UnknownSymbol("__xxxyyyzzz__".to_owned()))
        );
    }

    // A trace which contains a call to something which we don't have SIR for should emit a TIR
    // call operation.
    #[test]
    fn call_symbol_tir() {
        struct IO(());
        #[interp_step]
        fn interp_step(_: &mut IO) {
            let _ = unsafe { add6(1, 1, 1, 1, 1, 1) };
        }

        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut IO(()));
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        assert_tir(
            "...\n\
            ops:\n\
              %a = call(add6, [1u64, 1u64, 1u64, 1u64, 1u64, 1u64])\n\
              ...
              dead(%a)\n\
              ...",
            &tir_trace,
        );
    }

    /// Execute a trace which calls a symbol accepting no arguments, but which does return a value.
    #[test]
    fn exec_call_symbol_no_args() {
        struct IO(u32);
        #[interp_step]
        fn interp_step(io: &mut IO) {
            io.0 = unsafe { getuid() };
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let mut args = IO(0);
        TraceCompiler::<IO>::compile(tir_trace).execute(&mut args);
        assert_eq!(inputs.0, args.0);
    }

    /// Execute a trace which calls a symbol accepting arguments and returns a value.
    #[test]
    fn exec_call_symbol_with_arg() {
        struct IO(i32);
        #[interp_step]
        fn interp_step(io: &mut IO) {
            io.0 = unsafe { abs(io.0) };
        }

        let mut inputs = IO(-56);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let mut args = IO(-56);
        TraceCompiler::<IO>::compile(tir_trace).execute(&mut args);
        assert_eq!(inputs.0, args.0);
    }

    /// The same as `exec_call_symbol_args_with_rv`, just using a constant argument.
    #[test]
    fn exec_call_symbol_with_const_arg() {
        struct IO(i32);
        #[interp_step]
        fn interp_step(io: &mut IO) {
            io.0 = unsafe { abs(-123) };
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let mut args = IO(0);
        TraceCompiler::<IO>::compile(tir_trace).execute(&mut args);
        assert_eq!(inputs.0, args.0);
    }

    #[test]
    fn exec_call_symbol_with_many_args() {
        struct IO(u64);
        #[interp_step]
        fn interp_step(io: &mut IO) {
            io.0 = unsafe { add6(1, 2, 3, 4, 5, 6) };
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let mut args = IO(0);
        TraceCompiler::<IO>::compile(tir_trace).execute(&mut args);
        assert_eq!(inputs.0, 21);
        assert_eq!(inputs.0, args.0);
    }

    #[test]
    fn exec_call_symbol_with_many_args_some_ignored() {
        struct IO(u64);
        #[interp_step]
        fn interp_step(io: &mut IO) {
            io.0 = unsafe { add_some(1, 2, 3, 4, 5) };
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let mut args = IO(0);
        TraceCompiler::<IO>::compile(tir_trace).execute(&mut args);
        assert_eq!(args.0, 7);
        assert_eq!(args.0, inputs.0);
    }

    #[ignore] // FIXME: It has become hard to test spilling.
    #[test]
    fn test_spilling_simple() {
        struct IO(u64);

        #[interp_step]
        fn many_locals(io: &mut IO) {
            let _a = 1;
            let _b = 2;
            let _c = 3;
            let _d = 4;
            let _e = 5;
            let _f = 6;
            let h = 7;
            let _g = true;
            io.0 = h;
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        many_locals(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let (ct, spills) = TraceCompiler::<IO>::test_compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 7);
        assert_eq!(spills, 3); // Three u8s.
    }

    #[ignore] // FIXME: It has become hard to test spilling.
    #[test]
    fn test_spilling_u64() {
        struct IO(u64);

        fn u64value() -> u64 {
            // We need an extra function here to avoid SIR optimising this by assigning assigning the
            // constant directly to the return value (which is a register).
            4294967296 + 8
        }

        #[inline(never)]
        #[interp_step]
        fn spill_u64(io: &mut IO) {
            let _a = 1;
            let _b = 2;
            let _c = 3;
            let _d = 4;
            let _e = 5;
            let _f = 6;
            let _g = 7;
            let h: u64 = u64value();
            io.0 = h;
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        spill_u64(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let (ct, spills) = TraceCompiler::<IO>::test_compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 4294967296 + 8);
        assert_eq!(spills, 2 * 8);
    }

    #[ignore] // FIXME: It has become hard to test spilling.
    #[test]
    fn test_mov_register_to_stack() {
        struct IO(u8, u8);

        #[interp_step]
        fn register_to_stack(io: &mut IO) {
            let _a = 1;
            let _b = 2;
            let _c = 3;
            let _d = 4;
            let _e = 5;
            let _f = 6;
            let _g = 7;
            let h = io.0;
            io.1 = h;
        }

        let mut inputs = IO(8, 0);
        let th = start_tracing(TracingKind::HardwareTracing);
        register_to_stack(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let (ct, spills) = TraceCompiler::<IO>::test_compile(tir_trace);
        let mut args = IO(8, 0);
        ct.execute(&mut args);
        assert_eq!(args.1, inputs.1);
        assert_eq!(spills, 9); // f, g: i32, h:  u8.
    }

    #[ignore] // FIXME: It has become hard to test spilling.
    #[test]
    fn test_mov_stack_to_register() {
        struct IO(u8);

        #[interp_step]
        fn stack_to_register(io: &mut IO) {
            let _a = 1;
            let _b = 2;
            let c = 3;
            let _d = 4;
            // When returning from `farg` all registers are full, so `e` needs to be allocated on the
            // stack. However, after we have returned, anything allocated during `farg` is freed. Thus
            // returning `e` will allocate a new local in a (newly freed) register, resulting in a `mov
            // reg, [rbp]` instruction.
            let e = farg(c);
            io.0 = e;
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        stack_to_register(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let (ct, spills) = TraceCompiler::<IO>::test_compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 3);
        assert_eq!(spills, 1); // Just one u8.
    }

    #[test]
    fn ext_call_and_spilling() {
        struct IO(u64);

        #[interp_step]
        fn ext_call(io: &mut IO) {
            let a = 1;
            let b = 2;
            let c = 3;
            let d = 4;
            let e = 5;
            // When calling `add_some` argument `a` is loaded from a register, while the remaining
            // arguments are loaded from the stack.
            let expect = unsafe { add_some(a, b, c, d, e) };
            io.0 = expect;
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        ext_call(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let mut args = IO(0);
        TraceCompiler::<IO>::compile(tir_trace).execute(&mut args);
        assert_eq!(inputs.0, 7);
        assert_eq!(inputs.0, args.0);
    }

    /// FIXME: New IR binop adds the same operands for some reason.
    #[test]
    fn test_binop_add_simple() {
        #[derive(Eq, PartialEq, Debug)]
        struct IO(u64, u64, u64);

        #[interp_step]
        fn interp_stepx(io: &mut IO) {
            io.2 = io.0 + io.1 + 3;
        }

        let mut inputs = IO(5, 2, 0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_stepx(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        println!("{}", tir_trace);
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut args = IO(5, 2, 0);
        ct.execute(&mut args);
        assert_eq!(args, IO(5, 2, 10));
    }

    // Similar test to the above, but makes sure the operations will be executed on the stack by
    // filling up all registers first.
    //#[test]
    //fn test_binop_add_stack() {
    //    struct IO(u8, u64);

    //    #[interp_step]
    //    fn interp_step(io: &mut IO) {
    //        let _a = 1;
    //        let _b = 2;
    //        let _c = 3;
    //        let _d = 4;
    //        let _e = 5;
    //        let _d = 6;
    //        io.0 = add(13);
    //        io.1 = add64(1);
    //    }

    //    let mut inputs = IO(0, 0);
    //    let th = start_tracing(TracingKind::HardwareTracing);
    //    interp_step(&mut inputs);
    //    let sir_trace = th.stop_tracing().unwrap();
    //    let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
    //    let ct = TraceCompiler::<IO>::compile(tir_trace);
    //    let mut args = IO(0, 0);
    //    ct.execute(&mut args);
    //    assert_eq!(args.0, 29);
    //    assert_eq!(args.1, 8589934593);
    //}

    #[test]
    fn field_projection_tir() {
        struct IO(u64);

        struct S {
            _x: u64,
            y: u64,
        }

        fn get_y(s: S) -> u64 {
            s.y
        }

        #[interp_step]
        fn interp_step(io: &mut IO) {
            let s = S { _x: 100, y: 200 };
            io.0 = get_y(s);
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();

        // %s1: Initial s in the outer function
        // %s2: A copy of s. Uninteresting.
        // %s3: s inside the function.
        // %res: the result of the call.
        assert_tir("
            local_decls:
              ...
              %s1: (%cgu, %tid1) => StructTy { offsets: [0, 8], tys: [(%cgu, %tid2), (%cgu, %tid2)], align: 8, size: 16 }
              ...
              %res: (%cgu, %tid2) => u64
              ...
              %s2: (%cgu, %tid1)...
              ...
              %s3: (%cgu, %tid1)...
              ...
            ops:
              ...
              (%s1).0 = 100u64
              (%s1).1 = 200u64
              ...
              %s2 = %s1
              ...
              enter(...
              ...
              %res = (%s3).1
              ...
              leave
              ...", &tir_trace);
    }

    #[test]
    fn test_ref_deref_simple() {
        struct IO(u64);

        #[interp_step]
        fn interp_step(io: &mut IO) {
            let mut x = 9;
            let y = &mut x;
            *y = 10;
            let z = *y;
            io.0 = z
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        println!("{}", tir_trace);
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 10);
    }

    #[test]
    fn test_ref_deref_stack() {
        struct IO(u64);

        #[interp_step]
        fn interp_step(io: &mut IO) {
            let _a = 1;
            let _b = 2;
            let _c = 3;
            let _d = 4;
            let _e = 5;
            let _f = 6;
            let mut x = 9;
            let y = &mut x;
            *y = 10;
            let z = *y;
            io.0 = z
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 10);
    }

    /// Dereferences a variable that lives on the stack and stores it in a register.
    #[test]
    fn test_deref_stack_to_register() {
        fn deref1(arg: u64) -> u64 {
            let a = &arg;
            return *a;
        }

        #[interp_step]
        fn interp_step(io: &mut IO) {
            let _a = 1;
            let _b = 2;
            let _c = 3;
            let f = 6;
            io.0 = deref1(f);
        }

        struct IO(u64);
        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 6);
    }

    #[test]
    fn test_deref_register_to_stack() {
        struct IO(u64);

        fn deref2(arg: u64) -> u64 {
            let a = &arg;
            let _b = 2;
            let _c = 3;
            let _d = 4;
            return *a;
        }

        #[interp_step]
        fn interp_step(io: &mut IO) {
            let f = 6;
            io.0 = deref2(f);
        }

        // This test dereferences a variable that lives on the stack and stores it in a register.
        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 6);
    }

    #[test]
    fn test_do_not_trace() {
        struct IO(u8);

        #[do_not_trace]
        fn dont_trace_this(a: u8) -> u8 {
            let b = 2;
            let c = a + b;
            c
        }

        #[interp_step]
        fn interp_step(io: &mut IO) {
            io.0 = dont_trace_this(io.0);
        }

        let mut inputs = IO(1);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();

        assert_tir(
            "
            local_decls:
              ...
            ops:
              ...
              %s1 = call(...
              ...",
            &tir_trace,
        );

        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut args = IO(1);
        ct.execute(&mut args);
        assert_eq!(args.0, 3);
    }

    #[test]
    fn test_do_not_trace_stdlib() {
        struct IO<'a>(&'a mut Vec<u64>);

        #[interp_step]
        fn dont_trace_stdlib(io: &mut IO) {
            io.0.push(3);
        }

        let mut vec: Vec<u64> = Vec::new();
        let mut inputs = IO(&mut vec);
        let th = start_tracing(TracingKind::HardwareTracing);
        dont_trace_stdlib(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut argv: Vec<u64> = Vec::new();
        let mut args = IO(&mut argv);
        ct.execute(&mut args);
        assert_eq!(argv.len(), 1);
        assert_eq!(argv[0], 3);
    }

    #[test]
    fn test_projection_chain() {
        #[derive(Debug)]
        struct IO((usize, u8, usize), u8, S, usize);

        #[derive(Debug, PartialEq)]
        struct S {
            x: usize,
            y: usize,
        }

        #[interp_step]
        fn interp_step(io: &mut IO) {
            io.1 = (io.0).1;
            io.3 = io.2.y;
        }

        let s = S { x: 5, y: 6 };
        let t = (1, 2, 3);
        let mut inputs = IO(t, 0u8, s, 0usize);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);

        let t2 = (1, 2, 3);
        let s2 = S { x: 5, y: 6 };
        let mut args = IO(t2, 0u8, s2, 0usize);
        ct.execute(&mut args);
        assert_eq!(args.0, (1usize, 2u8, 3usize));
        assert_eq!(args.1, 2u8);
        assert_eq!(args.2, S { x: 5, y: 6 });
        assert_eq!(args.3, 6);
    }

    #[test]
    fn test_projection_lhs() {
        struct IO((u8, u8), u8);

        #[interp_step]
        fn interp_step(io: &mut IO) {
            (io.0).1 = io.1;
        }

        let t = (1u8, 2u8);
        let mut inputs = IO(t, 3u8);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let t2 = (1u8, 2u8);
        let mut args = IO(t2, 3u8);
        ct.execute(&mut args);
        assert_eq!((args.0).1, 3);
    }

    #[test]
    fn test_array() {
        struct IO<'a>(&'a mut [u8; 3], u8);

        #[interp_step]
        #[inline(never)]
        fn array(io: &mut IO) {
            let z = io.0[1];
            io.1 = z;
        }

        let mut a = [3, 4, 5];
        let mut inputs = IO(&mut a, 0);
        let th = start_tracing(TracingKind::HardwareTracing);
        array(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        assert_eq!(inputs.1, 4);
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut a2 = [3, 4, 5];
        let mut args = IO(&mut a2, 0);
        ct.execute(&mut args);
        assert_eq!(args.1, 4);
    }

    /// Test codegen of field access on a struct ref on the right-hand side.
    #[test]
    fn rhs_struct_ref_field() {
        struct IO(u8);

        #[interp_step]
        fn add1(io: &mut IO) {
            io.0 = io.0 + 1
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        add1(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);

        let mut args = IO(10);
        ct.execute(&mut args);
        assert_eq!(args.0, 11);
    }

    /// Test codegen of indexing a struct ref on the left-hand side.
    #[test]
    fn mut_lhs_struct_ref() {
        struct IO(u8);

        #[interp_step]
        fn set100(io: &mut IO) {
            io.0 = 100;
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        set100(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);

        let mut args = IO(10);
        ct.execute(&mut args);
        assert_eq!(args.0, 100);
    }

    /// Test codegen of copying something which doesn't fit in a register.
    #[test]
    fn place_larger_than_reg() {
        #[derive(Debug, Eq, PartialEq)]
        struct S(u64, u64, u64);
        struct IO(S);

        #[interp_step]
        fn ten(io: &mut IO) {
            io.0 = S(10, 10, 10);
        }

        let mut inputs = IO(S(0, 0, 0));
        let th = start_tracing(TracingKind::HardwareTracing);
        ten(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        assert_eq!(inputs.0, S(10, 10, 10));

        let mut args = IO(S(1, 1, 1));
        ct.execute(&mut args);
        assert_eq!(args.0, S(10, 10, 10));
    }

    #[test]
    #[ignore] // FIXME Broken during new trimming scheme. Seg faults.
    fn test_rvalue_len() {
        struct IO<'a>(&'a [u8], u8);

        fn matchthis(inputs: &IO, pc: usize) -> u8 {
            let x = match inputs.0[pc] as char {
                'a' => 1,
                'b' => 2,
                _ => 0,
            };
            x
        }

        #[interp_step]
        fn interp_step(io: &mut IO) {
            let x = matchthis(&io, 0);
            io.1 = x;
        }

        let a = "abc".as_bytes();
        let mut inputs = IO(&a, 0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);
        let mut a2 = "abc".as_bytes();
        let mut args = IO(&mut a2, 0);
        ct.execute(&mut args);
        assert_eq!(args.1, 1);
    }

    // Only `interp_step` annotated functions and their callees should remain after trace trimming.
    #[test]
    fn trim_junk() {
        struct IO(u8);

        #[interp_step]
        fn interp_step(io: &mut IO) {
            io.0 += 1;
        }

        let mut inputs = IO(0);
        let th = start_tracing(TracingKind::HardwareTracing);
        interp_step(&mut inputs);
        inputs.0 = 0; // Should get trimmed.
        interp_step(&mut inputs);
        inputs.0 = 0; // Should get trimmed
        interp_step(&mut inputs);
        let sir_trace = th.stop_tracing().unwrap();
        let tir_trace = TirTrace::new(&*SIR, &*sir_trace).unwrap();
        let ct = TraceCompiler::<IO>::compile(tir_trace);

        let mut args = IO(0);
        ct.execute(&mut args);
        assert_eq!(args.0, 3);
    }
}
