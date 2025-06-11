//! The Yk PT trace decoder.
//!
//! The decoder works in two modes:
//!
//!  - compiler-assisted decoding
//!  - disassembly-based decoding
//!
//! The former mode is for decoding portions of the trace that are for "native code" (code compiled
//! by ykllvm). Code built with ykllvm gets static control flow information embedded in the end
//! binary in a special section. Along with dynamic information provided by the PT packet stream,
//! we have everything we need to decode these parts of the trace without disassembling
//! instructions. This is the preferred mode of decoding.
//!
//! The latter mode is a fallback for decoding portions of the trace that are for "foreign code"
//! (code not built with ykllvm). For foreign code there is no static control flow edge information
//! available, so to decode these parts of the trace we have to disassemble the instruction stream
//! (like libipt does).
//!
//! You may now be asking: why not just skip the parts of the trace that are for foreign code?
//! After all, if a portion of code wasn't built with ykllvm, then we won't have IR for it anyway,
//! meaning that the JIT is unable to inline it and we'd have to emit a call into the JIT trace.
//! Why bother decoding the bit of the trace we don't actually care about?
//!
//! The problem is, it's not always easy to identify which parts of a PT trace are for native or
//! foreign code: it's easy if the CPU that doesn't implement the deferred TIP optimisation (see
//! the Intel 64 and IA32 Architectures Software Developer's Manual, Vol 3, Section  32.4.2.3).
//!
//! Deferred TIPs mean that TNT decisions can come in "out of order" with TIP updates. For example,
//! a packet stream `[TNT(0,1), TIP(addr), TNT(1,0)]` may be optimised to `[TNT(0, 1, 1, 0),
//! TIP(addr)]`. If the TIP update to `addr` is the return from foreign code, then when we resume
//! compiler-assisted decoding then we need only the last two TNT decisions in the buffer
//! (discarding the first two as we skip over foreign code). The problem is that without successor
//! block information about the foreign code we can't know how how many TNT decisions correspond
//! with the foreign code, and thus how many decisions to discard.
//!
//! We therefore have to disassemble foreign code, popping TNT decisions as we encounter
//! conditional branch instructions. We can still use compiler-assisted decoding for portions of
//! code that are compiled with ykllvm.

mod packets;
mod parser;

use crate::{
    errors::{HWTracerError, TemporaryErrorKind},
    llvm_blockmap::{BlockMapEntry, SuccessorKind, LLVM_BLOCK_MAP},
    perf::collect::PerfTraceBuf,
    Block, BlockIteratorError,
};
use intervaltree::IntervalTree;
use std::{
    collections::VecDeque,
    fmt::{self, Debug},
    ops::Range,
    path::PathBuf,
    slice,
    sync::LazyLock,
};
use thiserror::Error;
use ykaddr::obj::{PHDR_OBJECT_CACHE, SELF_BIN_PATH};

use packets::{Bitness, Packet, PacketKind};
use parser::PacketParser;

/// The virtual address ranges of segments that we may need to disassemble.
static CODE_SEGS: LazyLock<CodeSegs> = LazyLock::new(|| {
    let mut segs = Vec::new();
    for obj in PHDR_OBJECT_CACHE.iter() {
        let obj_base = obj.addr();
        for hdr in obj.phdrs() {
            if (hdr.flags() & libc::PF_W) == 0 {
                let vaddr = usize::try_from(obj_base + hdr.vaddr()).unwrap();
                let memsz = usize::try_from(hdr.memsz()).unwrap();
                let key = vaddr..(vaddr + memsz);
                segs.push((key.clone(), ()));
            }
        }
    }
    let tree = segs.into_iter().collect::<IntervalTree<usize, ()>>();
    CodeSegs { tree }
});

/// The number of compressed returns that a CPU implementing Intel Processor Trace can keep track
/// of. This is a bound baked into the hardware, but the decoder needs to be aware of it for its
/// compressed return stack.
const PT_MAX_COMPRETS: usize = 64;

/// A data structure providing convenient access to virtual address ranges and memory slices for
/// segments.
///
/// FIXME: For now this assumes that no dlopen()/dlclose() is happening.
struct CodeSegs {
    tree: IntervalTree<usize, ()>,
}

impl CodeSegs {
    /// Obtain the virtual address range and a slice of memory for the segment containing the
    /// specified virtual address.
    fn seg<'s: 'a, 'a>(&'s self, vaddr: usize) -> Segment<'a> {
        let mut hits = self.tree.query(vaddr..(vaddr + 1));
        match hits.next() {
            Some(x) => {
                // Segments can't overlap.
                debug_assert_eq!(hits.next(), None);

                let slice = unsafe {
                    slice::from_raw_parts(x.range.start as *const u8, x.range.end - x.range.start)
                };
                Segment {
                    vaddrs: &x.range,
                    slice,
                }
            }
            None => todo!(), // Has an object been loaded or unloaded at runtime?
        }
    }
}

/// The virtual address range of and memory slice of one ELF segment.
struct Segment<'a> {
    /// The virtual address range of the segment.
    vaddrs: &'a Range<usize>,
    /// A memory slice for the segment.
    slice: &'a [u8],
}

/// Represents a location in the instruction stream of the traced binary.
#[derive(Eq, PartialEq)]
enum ObjLoc {
    /// A virtual address known to originate from the main executable object.
    MainObj(usize),
    /// Anything else, as a virtual address (if known).
    OtherObjOrUnknown(Option<usize>),
}

impl ObjLoc {
    /// Return the virtual address of a location.
    ///
    /// # Panics
    ///
    /// Panics if the virtual address is not known.
    fn vaddr(&self) -> usize {
        match self {
            Self::MainObj(v) => *v,
            Self::OtherObjOrUnknown(v) => v.unwrap(),
        }
    }
}

impl Debug for ObjLoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MainObj(v) => write!(f, "ObjLoc::MainObj(0x{v:x})"),
            Self::OtherObjOrUnknown(e) => {
                if let Some(e) = e {
                    write!(f, "ObjLoc::OtherObjOrUnknown(0x{e:x})")
                } else {
                    write!(f, "ObjLoc::OtherObjOrUnknown(???)")
                }
            }
        }
    }
}

/// The return addresses that can appear on the compressed return stack.
#[derive(Debug, Clone)]
enum CompRetAddr {
    /// A regular return address (as a virtual address).
    VAddr(usize),
    /// Return to directly after the callsite at the given virtual address in the main object
    /// binary.
    ///
    /// This exists because when we do compiler-assisted decoding, we don't disassemble the
    /// instruction stream, and thus we don't know how long the call instruction is, and hence nor
    /// the address of the instruction to return to. That's actually OK, because compiler-assisted
    /// decoding needs only to know after which call to continue decoding after.
    AfterCall(usize),
}

/// The compressed return stack (required for the compressed returns optimisation implemented by
/// some Intel CPUs).
///
/// In short, the call-chains of most programs are "well-behaved" in that their calls and returns
/// are properly nested. In such scenarios, it's not necessary for each return from a function to
/// report a fresh target IP (return address) via a PT packet, since the return address can be
/// inferred from an earlier callsite.
///
/// For more information, consult Section 34.4.2.2 of the Intel 64 and IA-32 Architectures Software
/// Developer’s Manual, Volume 3 (under the "Indirect Transfer Compression for Returns")
/// sub-heading.
struct CompressedReturns {
    rets: VecDeque<CompRetAddr>,
}

impl CompressedReturns {
    fn new() -> Self {
        Self {
            rets: VecDeque::new(),
        }
    }

    fn push(&mut self, ret: CompRetAddr) {
        assert!(self.rets.len() <= PT_MAX_COMPRETS);

        // The stack is fixed-size. When the stack is full and a new entry is pushed, the oldest
        // entry is evicted.
        if self.rets.len() == PT_MAX_COMPRETS {
            self.rets.pop_front();
        }

        self.rets.push_back(ret);
    }

    fn pop(&mut self) -> Option<CompRetAddr> {
        self.rets.pop_back()
    }
}

/// Iterate over the blocks of an Intel PT trace using the fast Yk PT decoder.
pub(crate) struct YkPTBlockIterator<'t> {
    trace: PerfTraceBuf,
    /// The packet iterator used to drive the decoding process.
    parser: PacketParser<'t>,
    /// Keeps track of where we are in the traced binary.
    cur_loc: ObjLoc,
    /// A vector of "taken/not-taken" (TNT) decisions. These arrive in batches and get buffered
    /// here in a FIFO fashion (oldest decision at head poistion).
    tnts: VecDeque<bool>,
    /// The compressed return stack.
    comprets: CompressedReturns,
    /// When `true` we have seen one of more `MODE.*` packets that are yet to be bound.
    unbound_modes: bool,
}

impl YkPTBlockIterator<'_> {
    pub(crate) fn new(trace: PerfTraceBuf, trace_len: usize) -> Self {
        // We must keep `self.trace` alive at least as long as `self.parser`
        let bytes = unsafe { slice::from_raw_parts(trace.0, trace_len) };
        Self {
            trace,
            parser: PacketParser::new(bytes),
            cur_loc: ObjLoc::OtherObjOrUnknown(None),
            tnts: VecDeque::new(),
            comprets: CompressedReturns::new(),
            unbound_modes: false,
        }
    }

    /// Convert a virtual address to a file offset.
    fn vaddr_to_off(&self, vaddr: usize) -> Result<(PathBuf, u64), IteratorError> {
        ykaddr::addr::vaddr_to_obj_and_off(vaddr).ok_or(IteratorError::NoSuchVAddr)
    }

    /// Looks up the blockmap entry for the given virtual address.
    fn lookup_blockmap_entry(
        &self,
        vaddr: usize,
    ) -> Option<&'static intervaltree::Element<usize, BlockMapEntry>> {
        let mut ents = LLVM_BLOCK_MAP.query(vaddr, vaddr + 1);
        if let Some(ent) = ents.next() {
            // A single-address range cannot span multiple blocks.
            debug_assert!(ents.next().is_none());
            Some(ent)
        } else {
            None
        }
    }

    // Lookup a block from the "main executable object" by it's virtual address.
    fn lookup_block_by_vaddr(&mut self, vaddr: usize) -> Result<Block, IteratorError> {
        if let Some(ent) = self.lookup_blockmap_entry(vaddr) {
            Ok(Block::from_vaddr_range(ent.range.start, ent.range.end))
        } else {
            Ok(Block::Unknown)
        }
    }

    /// Use the blockmap entry `ent` to follow the next (after the virtual address `b_vaddr`) call
    /// in the block (if one exists).
    ///
    /// Returns `Ok(Some(blk))` if there was a call to follow that lands us in the block `blk`.
    ///
    /// Returns `Ok(None)` if there was no call to follow after `b_vaddr`.
    fn maybe_follow_blockmap_call(
        &mut self,
        b_vaddr: usize,
        ent: &BlockMapEntry,
    ) -> Result<Option<Block>, IteratorError> {
        if let Some(call_info) = ent
            .call_vaddrs()
            .iter()
            .find(|c| c.callsite_vaddr() >= b_vaddr)
        {
            let target = call_info.target_vaddr();

            if let Some(target_vaddr) = target {
                // The address of the callee is known.
                //
                // PT won't compress returns from direct calls if the call target is the
                // instruction address immediately after the call.
                //
                // See the Intel Manual, Section 33.4.2.2 for details.
                if !call_info.is_direct() || target_vaddr != call_info.return_vaddr() {
                    self.comprets
                        .push(CompRetAddr::AfterCall(call_info.callsite_vaddr()));
                }
                // eprintln!("follow known call target to: {:x}", target_vaddr);
                if crate::perf::collect::YKTEXT_EXTENT.contains(&target_vaddr) {
                    self.cur_loc = ObjLoc::MainObj(target_vaddr);
                    return Ok(Some(self.lookup_block_by_vaddr(target_vaddr)?));
                } else {
                    // eprintln!("PT filtered out callee");
                    // We have a blockmap entry for the callee, but the function was filtered out
                    // at the PT level, so we won't see PT packets for it and we shouldn't allow
                    // compiler-assisted decoding to shoot off through this callee. Instead look
                    // for a TIP update that tells us where to resume decoding.
                    self.seek_tip()?;
                    return match self.cur_loc {
                        ObjLoc::MainObj(vaddr) => Ok(Some(self.lookup_block_by_vaddr(vaddr)?)),
                        ObjLoc::OtherObjOrUnknown(_) => Ok(Some(Block::Unknown)),
                    };
                }
            } else {
                // The address of the callee isn't known.
                // eprintln!("Follow call unknown");
                self.comprets
                    .push(CompRetAddr::AfterCall(call_info.callsite_vaddr()));
                // eprintln!("SEEKTIP");
                self.seek_tip()?;
                // eprintln!("/SEEKTIP");
                return match self.cur_loc {
                    ObjLoc::MainObj(vaddr) => Ok(Some(self.lookup_block_by_vaddr(vaddr)?)),
                    ObjLoc::OtherObjOrUnknown(_) => Ok(Some(Block::Unknown)),
                };
            }
        }

        Ok(None)
    }

    /// Find where to go next based on whether the trace took the branch(es) or not.
    fn follow_conditional_successor(
        &mut self,
        num_cond_brs: u8,
        taken_target: usize,
        not_taken_target: Option<usize>,
    ) -> Result<usize, IteratorError> {
        assert_ne!(num_cond_brs, 0);
        // Blocks with more than 2 terminating conditional branch instructions aren't an issue, but
        // it would be informative to know if/when that happens.
        assert!(num_cond_brs <= 2);

        // Control flow went to `taken_target` if any of the conditional branches were taken.
        let mut taken = false;
        for _ in 0..num_cond_brs {
            if self.tnts.is_empty() {
                self.seek_tnt()?;
            }
            if self.tnts.pop_front().unwrap() {
                taken = true;
                break;
            }
        }
        if taken {
            Ok(taken_target)
        } else if let Some(ntt) = not_taken_target {
            Ok(ntt)
        } else {
            // Divergent control flow.
            todo!();
        }
    }

    /// Follow the successor of the block described by the blockmap entry `ent`.
    fn follow_blockmap_successor(&mut self, ent: &BlockMapEntry) -> Result<Block, IteratorError> {
        match ent.successor() {
            SuccessorKind::Unconditional { target } => {
                // eprintln!("follow uncond");
                if let Some(target_vaddr) = target {
                    self.cur_loc = ObjLoc::MainObj(*target_vaddr);
                    self.lookup_block_by_vaddr(*target_vaddr)
                } else {
                    // Divergent control flow.
                    todo!();
                }
            }
            SuccessorKind::Conditional {
                num_cond_brs,
                taken_target,
                not_taken_target,
            } => {
                // eprintln!("follow cond");
                let target_vaddr = self.follow_conditional_successor(
                    *num_cond_brs,
                    *taken_target,
                    *not_taken_target,
                )?;
                self.cur_loc = ObjLoc::MainObj(target_vaddr);
                self.lookup_block_by_vaddr(target_vaddr)
            }
            SuccessorKind::Return => {
                // eprintln!("follow ret");
                if self.is_return_compressed()? {
                    // This unwrap cannot fail if the CPU has implemented compressed
                    // returns correctly.
                    self.cur_loc = match self.comprets.pop().unwrap() {
                        CompRetAddr::AfterCall(vaddr) => ObjLoc::MainObj(vaddr + 1),
                        CompRetAddr::VAddr(vaddr) => ObjLoc::MainObj(vaddr),
                    };
                    if let ObjLoc::MainObj(vaddr) = self.cur_loc {
                        self.lookup_block_by_vaddr(vaddr + 1)
                    } else {
                        Ok(Block::Unknown)
                    }
                } else {
                    // A regular uncompressed return that relies on a TIP update.
                    //
                    // Note that `is_return_compressed()` has already updated
                    // `self.cur_loc()`.
                    match self.cur_loc {
                        ObjLoc::MainObj(vaddr) => Ok(self.lookup_block_by_vaddr(vaddr)?),
                        ObjLoc::OtherObjOrUnknown(_) => Ok(Block::Unknown),
                    }
                }
            }
            SuccessorKind::Dynamic => {
                // eprintln!("follow dyn");
                // We can only know the successor via a TIP update in a packet.
                self.seek_tip()?;
                match self.cur_loc {
                    ObjLoc::MainObj(vaddr) => {
                        // eprintln!("dyn vaddr: {vaddr:x}");
                        if crate::perf::collect::YKTEXT_EXTENT.contains(&vaddr) {
                            // eprintln!("not filtered");
                        }
                        Ok(self.lookup_block_by_vaddr(vaddr)?)
                    },
                    ObjLoc::OtherObjOrUnknown(_) => {
                        // eprintln!("dyn vaddr: ???");
                        Ok(Block::Unknown)
                    }
                }
            }
        }
    }

    fn do_next(&mut self) -> Result<Block, IteratorError> {
        // dbg!(&self.cur_loc);
        // Read as far ahead as we can using static successor info encoded into the blockmap.
        // eprintln!("cur_loc: {:?}", self.cur_loc);
        // eprintln!("cur tnts: {:?}", self.tnts);
        match self.cur_loc {
            ObjLoc::MainObj(vaddr) => {
                // eprintln!("in: {:?}", ykaddr::addr::vaddr_to_sym_and_obj(vaddr));
                // We know where we are in the main object binary, so there's a chance that there's
                // a blockmap entry for this location (not all code from the main object binary
                // necessarily has blockmap info. e.g. PLT resolution routines).
                if let Some(ent) = self.lookup_blockmap_entry(vaddr) {
                    // If there are calls in the block that come *after* the current position in the
                    // block, then we will need to follow those before we look at the successor info.
                    // dbg!("LLL1");
                    if let Some(blk) = self.maybe_follow_blockmap_call(vaddr, &ent.value)? {
                        // dbg!("LLL2");
                        Ok(blk)
                    } else {
                        // dbg!("LLL3");
                        // If we get here, there were no further calls to follow in the block, so we
                        // consult the static successor information.
                        self.follow_blockmap_successor(&ent.value)
                    }
                } else {
                    self.cur_loc = ObjLoc::OtherObjOrUnknown(Some(vaddr));
                    Ok(Block::Unknown)
                }
            }
            ObjLoc::OtherObjOrUnknown(vaddr) => self.skip_foreign(vaddr),
        }
    }

    /// Returns the target virtual address for a branch instruction.
    fn branch_target_vaddr(&self, inst: &iced_x86::Instruction) -> u64 {
        match inst.op0_kind() {
            iced_x86::OpKind::NearBranch16 => inst.near_branch16().into(),
            iced_x86::OpKind::NearBranch32 => inst.near_branch32().into(),
            iced_x86::OpKind::NearBranch64 => inst.near_branch64(),
            iced_x86::OpKind::FarBranch16 | iced_x86::OpKind::FarBranch32 => panic!(),
            _ => unreachable!(),
        }
    }

    // Determines if a return from a function was compressed in the packet stream.
    //
    // In the event that the return is compressed, the taken decision is popped from `self.tnts`.
    fn is_return_compressed(&mut self) -> Result<bool, IteratorError> {
        let compressed = if !self.tnts.is_empty() {
            // As the Intel manual explains, when a return is *not* compressed, the CPU's TNT
            // buffers are flushed, so if we have any buffered TNT decisions, then this must be a
            // *compressed* return.
            true
        } else {
            // This *may* be a compressed return. If the next (non-out-of-context) event packet
            // carries a TIP update then this was an uncompressed return, otherwise it was
            // compressed.
            let pkt = self.seek_tnt_or_tip(true, true)?;
            pkt.tnts().is_some()
        };

        if compressed {
            // NOTE: if/when re-enabling compressed returns, see the git history for the logic to
            // insert here.
            unreachable!("compressed returns are disabled in collect.c");
        }

        Ok(compressed)
    }

    fn disassemble(&mut self, start_vaddr: usize) -> Result<Block, IteratorError> {
        let mut seg = CODE_SEGS.seg(start_vaddr);
        let mut dis =
            iced_x86::Decoder::with_ip(64, seg.slice, u64::try_from(seg.vaddrs.start).unwrap(), 0);
        dis.set_ip(u64::try_from(start_vaddr).unwrap());
        dis.set_position(start_vaddr - seg.vaddrs.start).unwrap();
        let mut reposition: bool = false;

        loop {
            let vaddr = usize::try_from(dis.ip()).unwrap();
            let (obj, _) = self.vaddr_to_off(vaddr)?;

            if obj == *SELF_BIN_PATH {
                let block = self.lookup_block_by_vaddr(vaddr)?;
                if !block.is_unknown() {
                    // We are back to "native code" and can resume compiler-assisted decoding.
                    self.cur_loc = ObjLoc::MainObj(vaddr);
                    return Ok(block);
                }
            }

            if !seg.vaddrs.contains(&vaddr) {
                // The next instruction is outside of the current segment. Switch segment and make
                // a new decoder for it.
                seg = CODE_SEGS.seg(vaddr);
                let seg_start_u64 = u64::try_from(seg.vaddrs.start).unwrap();
                dis = iced_x86::Decoder::with_ip(64, seg.slice, seg_start_u64, 0);
                dis.set_ip(u64::try_from(vaddr).unwrap());
                reposition = true;
            }

            if reposition {
                dis.set_position(vaddr - seg.vaddrs.start).unwrap();
                reposition = false;
            }

            let inst = dis.decode();
            match inst.flow_control() {
                iced_x86::FlowControl::Next => (),
                iced_x86::FlowControl::Return => {
                    // We don't expect to see any 16-bit far returns.
                    debug_assert!(is_ret_near(&inst));

                    let ret_vaddr = if self.is_return_compressed()? {
                        // This unwrap cannot fail if the CPU correctly implements compressed
                        // returns.
                        match self.comprets.pop().unwrap() {
                            CompRetAddr::VAddr(vaddr) => vaddr,
                            CompRetAddr::AfterCall(vaddr) => vaddr + 1,
                        }
                    } else {
                        self.cur_loc.vaddr()
                    };
                    dis.set_ip(u64::try_from(ret_vaddr).unwrap());
                    reposition = true;
                }
                iced_x86::FlowControl::IndirectBranch | iced_x86::FlowControl::IndirectCall => {
                    self.seek_tip()?;
                    let vaddr = self.cur_loc.vaddr();
                    if inst.flow_control() == iced_x86::FlowControl::IndirectCall {
                        debug_assert!(!inst.is_call_far());
                        // Indirect calls, even zero-length ones, are always compressed. See
                        // Section 33.4.2.2 of the Intel Manual:
                        //
                        // "push the next IP onto the stack...note that this excludes zero-length
                        // CALLs, which are *direct* near CALLs with displacement zero (to the next
                        // IP)
                        self.comprets
                            .push(CompRetAddr::VAddr(usize::try_from(inst.next_ip()).unwrap()));
                    }

                    dis.set_ip(u64::try_from(vaddr).unwrap());
                    reposition = true;
                }
                iced_x86::FlowControl::ConditionalBranch => {
                    // Ensure we have TNT decisions buffered.
                    if self.tnts.is_empty() {
                        self.seek_tnt()?;
                    }
                    // unwrap() cannot fail as the above code ensures we have decisions buffered.
                    if self.tnts.pop_front().unwrap() {
                        dis.set_ip(self.branch_target_vaddr(&inst));
                        reposition = true;
                    }
                }
                iced_x86::FlowControl::UnconditionalBranch => {
                    dis.set_ip(self.branch_target_vaddr(&inst));
                    reposition = true;
                }
                iced_x86::FlowControl::Call => {
                    // A *direct* call.
                    if inst.code() == iced_x86::Code::Syscall {
                        // Do nothing. We have disabled kernel tracing in hwtracer, so
                        // entering/leaving a syscall will generate packet generation
                        // disable/enable events (`TIP.PGD`/`TIP.PGE` packets) which are handled by
                        // the decoder elsewhere.
                    } else {
                        let target_vaddr = self.branch_target_vaddr(&inst);

                        // Intel PT doesn't compress a direct call to the next instruction.
                        //
                        // Section 33.4.2.2 of the Intel Manual:
                        //
                        // "For near CALLs, push the Next IP onto the stack... Note that this
                        // excludes zero-length CALLs, which are direct near CALLs with
                        // displacement zero (to the next IP).
                        if target_vaddr != inst.next_ip() {
                            self.comprets
                                .push(CompRetAddr::VAddr(usize::try_from(inst.next_ip()).unwrap()));
                        }
                        // We don't expect to see any 16-bit mode far calls in modernity.
                        debug_assert!(!inst.is_call_far());
                        dis.set_ip(target_vaddr);
                        reposition = true;
                    }
                }
                iced_x86::FlowControl::Interrupt => {
                    // It's my understanding that `INT` instructions aren't really used any more.
                    // Interrupt 0x80 used to be used to do system calls, but now there is the
                    // `SYSCALL` instruction which is generally preferred.
                    unreachable!("interrupt");
                }
                iced_x86::FlowControl::XbeginXabortXend => {
                    // Transactions. These are a bit like time machines for the CPU. They can cause
                    // memory and registers to be rewound to a (dynamically decided) past state.
                    //
                    // FIXME: We might be able to handle these by peeking ahead in the trace, but
                    // let's cross that bridge when we come to it.
                    todo!("transaction instruction: {}", inst);
                }
                iced_x86::FlowControl::Exception => {
                    // We were unable to disassemble the instruction stream to a valid x86_64
                    // instruction. This shouldn't happen, and if it does, I want to know about it!
                    unreachable!("invalid instruction encoding");
                }
            }
        }
    }

    /// Skip over "foreign code" for which we have no blockmap info for.
    fn skip_foreign(&mut self, start_vaddr: Option<usize>) -> Result<Block, IteratorError> {
        let start_vaddr = match start_vaddr {
            Some(v) => v,
            None => {
                // We don't statically know where to start, so we rely on a TIP update to tell us.
                self.seek_tip()?;
                match self.cur_loc {
                    ObjLoc::OtherObjOrUnknown(Some(vaddr)) => vaddr,
                    ObjLoc::OtherObjOrUnknown(None) => {
                        // The above `seek_tip()` ensures this can't happen!
                        unreachable!();
                    }
                    ObjLoc::MainObj(vaddr) => {
                        // It's possible that the above `self.seek_tip()` has already landed us
                        // back into mappable code (e.g. it skiped over a TIP.PGD and landed on a
                        // TIP.PGE at a mappable block).
                        let block = self.lookup_block_by_vaddr(vaddr)?;
                        if !block.is_unknown() {
                            return Ok(block);
                        }
                        // Otherwise we really do have to resort to disassembly.
                        vaddr
                    }
                }
            }
        };
        self.disassemble(start_vaddr)
    }

    /// Keep decoding packets until we encounter a TNT packet.
    fn seek_tnt(&mut self) -> Result<(), IteratorError> {
        loop {
            let pkt = self.packet()?; // Potentially populates `self.tnts`.
            if pkt.tnts().is_some() {
                return Ok(());
            }
        }
    }

    /// Keep decoding packets until we encounter one with a TIP update.
    ///
    /// This function does not specially handle out of context TIP packets: it will happily return
    /// out of context TIPs.
    ///
    /// TIP.PGD packets are skipped over since they mark the beggining of untraced code, and thus
    /// their TIP updates are not useful to us.
    fn seek_tip(&mut self) -> Result<(), IteratorError> {
        loop {
            // eprintln!("XXX");
            let pkt = self.packet()?;
            // dbg!(&pkt);
            // eprintln!("YYY");
            if pkt.kind().encodes_target_ip() && pkt.kind() != PacketKind::TIPPGD {
                // Note that self.packet() will have updated `self.cur_loc`.
                return Ok(());
            }
        }
    }

    /// Keep decoding packets until we encounter either a TNT packet or one with a TIP update.
    ///
    /// The packet is returned so that the consumer can determine which kind of packet was
    /// encountered.
    ///
    /// `skip_ooc` determines whether the caller wishes to skip over "Out Of Context" TIP packets
    /// or not.
    ///
    /// XXX
    /// Unlike `seek_tip` this function does not skip over TIP.PGD packets.
    ///
    /// FIXME: ^this is a bit confusing. Maybe we should add flags to both of these functions and
    /// let the call-sites decide if they care for TIP.PGD or not.
    fn seek_tnt_or_tip(&mut self, skip_ooc: bool, skip_pgd: bool) -> Result<Packet, IteratorError> {
        loop {
            // dbg!("AAA");
            let pkt = self.packet()?;
            // dbg!("BBB");
            if pkt.tnts().is_some()
                || (pkt.kind().encodes_target_ip() && (pkt.kind() != PacketKind::TIPPGD || !skip_pgd) && (pkt.target_ip().is_some() || !skip_ooc))
            {
                return Ok(pkt);
            }
        }
    }

    /// Skip packets up until and including the next `PSBEND` packet. The first packet after the
    /// `PSBEND` is returned.
    fn skip_psb_plus(&mut self) -> Result<Packet, IteratorError> {
        loop {
            if let Some(pkt_or_err) = self.parser.next() {
                if pkt_or_err?.kind() == PacketKind::PSBEND {
                    break;
                }
            } else {
                panic!("No more packets");
            }
        }

        if let Some(pkt_or_err) = self.parser.next() {
            Ok(pkt_or_err?)
        } else {
            Err(IteratorError::NoMorePackets)
        }
    }

    /// Fetch the next packet and update iterator state.
    fn packet(&mut self) -> Result<Packet, IteratorError> {
        // dbg!("packet");
        if let Some(pkt_or_err) = self.parser.next() {
            // dbg!("is more");
            let mut pkt = pkt_or_err?;

            if pkt.kind() == PacketKind::OVF {
                return Err(IteratorError::HWTracerError(HWTracerError::Temporary(
                    TemporaryErrorKind::TraceBufferOverflow,
                )));
            }

            if pkt.kind() == PacketKind::FUP && !self.unbound_modes {
                // dbg!("FUP");
                // FIXME: https://github.com/ykjit/yk/issues/593
                //
                // A FUP packet when there are no outstanding MODE packets indicates that
                // regular control flow was interrupted by an asynchronous event (e.g. a signal
                // handler or a context switch). For now we only support the simple case where
                // execution jumps off to some untraceable foreign code for a while, before
                // returning and resuming where we left off. This is characterised by a [FUP,
                // TIP.PGD, TIP.PGE] sequence (with no intermediate TIP or TNT packets). In
                // this case we can simply ignore the interruption. Later we need to support
                // FUPs more generally.
                pkt = self.seek_tnt_or_tip(false, false)?;
                if pkt.kind() != PacketKind::TIPPGD {
                    return Err(IteratorError::HWTracerError(HWTracerError::Temporary(
                        TemporaryErrorKind::TraceInterrupted,
                    )));
                }
                pkt = self.seek_tnt_or_tip(true, false)?;
                if pkt.kind() != PacketKind::TIPPGE {
                    return Err(IteratorError::HWTracerError(HWTracerError::Temporary(
                        TemporaryErrorKind::TraceInterrupted,
                    )));
                }
                if let Some(pkt_or_err) = self.parser.next() {
                    pkt = pkt_or_err?;
                } else {
                    return Err(IteratorError::NoMorePackets);
                }
            }

            // Section 33.3.7 of the Intel Manual says that packets in a PSB+ sequence:
            //
            //   "should be interpreted as "status only", since they do not imply any change of
            //   state at the time of the PSB, nor are they associated directly with any
            //   instruction or event. Thus, the normal binding and ordering rules that apply to
            //   these packets outside of PSB+ can be ignored..."
            //
            // So we don't let (e.g.) packets carrying a target ip inside a PSB+ update
            // `self.cur_loc`.
            if pkt.kind() == PacketKind::PSB {
                // Section 33.3.7 of the Intel Manual explains that:
                //
                //   "the decoder should never need to retain any information (e.g., LastIP,
                //   call stack, compound packet event) across a PSB; all compound packet
                //   events will be completed before a PSB, and any compression state will
                //   be reset"
                self.cur_loc = ObjLoc::OtherObjOrUnknown(None);
                self.comprets.rets.clear();
                assert!(self.tnts.is_empty());

                pkt = self.skip_psb_plus()?;
                if pkt.kind() == PacketKind::PSB {
                    todo!("psb+ followed by psb+");
                }
            }

            // If it's a MODE packet, remember we've seen it. The meaning of TIP and FUP packets
            // vary depending upon if they were preceded by MODE packets.
            if pkt.kind().is_mode() {
                // This whole codebase assumes 64-bit mode.
                if let Packet::MODEExec(ref mep) = pkt {
                    debug_assert_eq!(mep.bitness(), Bitness::Bits64);
                }
                self.unbound_modes = true;
            }

            // Does this packet bind to prior MODE packets? If so, it "consumes" the packet.
            if pkt.kind().encodes_target_ip() && self.unbound_modes {
                self.unbound_modes = false;
            }

            // Update `self.target_ip` if necessary.
            if let Some(vaddr) = pkt.target_ip() {
                self.cur_loc = match self.vaddr_to_off(vaddr)? {
                    (obj, _) if obj == *SELF_BIN_PATH => ObjLoc::MainObj(vaddr),
                    _ => ObjLoc::OtherObjOrUnknown(Some(vaddr)),
                };
            }

            // Update `self.tnts` if necessary.
            if let Some(bits) = pkt.tnts() {
                self.tnts.extend(bits);
            }

            Ok(pkt)
        } else {
            // dbg!("no more");
            return Err(IteratorError::NoMorePackets);
        }
    }
}

impl Iterator for YkPTBlockIterator<'_> {
    type Item = Result<Block, BlockIteratorError>;

    fn next(&mut self) -> Option<Self::Item> {
        // dbg!("NEXT");
        match self.do_next() {
            Ok(b) => Some(Ok(b)),
            Err(IteratorError::NoMorePackets) => {
                // dbg!("no more packets top");
                None
            }
            Err(IteratorError::NoSuchVAddr) => Some(Err(BlockIteratorError::NoSuchVAddr)),
            Err(IteratorError::HWTracerError(e)) => Some(Err(BlockIteratorError::HWTracerError(e))),
        } // .inspect(|x| { dbg!(&x); })
    }
}

impl Drop for YkPTBlockIterator<'_> {
    fn drop(&mut self) {
        // FIXME: `self.parser` is technically active at this point, and it still has a `&`
        // reference to `self.trace`. For example, `self.parser.drop` method could be called and do
        // something which relies on the memory which we're freeing here.
        unsafe { libc::free(self.trace.0 as *mut std::ffi::c_void) };
    }
}

/// An internal-to-this-module struct which allows the block iterator to distinguish "we reached
/// the end of the packet stream in an expected manner" from more serious errors.
#[derive(Debug, Error)]
enum IteratorError {
    #[error("No more packets")]
    NoMorePackets,
    #[cfg(ykpt)]
    #[error("No such vaddr")]
    NoSuchVAddr,
    #[error("HWTracerError: {0}")]
    HWTracerError(HWTracerError),
}

impl From<HWTracerError> for IteratorError {
    fn from(e: HWTracerError) -> Self {
        IteratorError::HWTracerError(e)
    }
}

/// iced_x86 should be providing this:
/// https://github.com/icedland/iced/issues/366
fn is_ret_near(inst: &iced_x86::Instruction) -> bool {
    debug_assert_eq!(inst.flow_control(), iced_x86::FlowControl::Return);
    use iced_x86::Code::*;
    match inst.code() {
        Retnd | Retnd_imm16 | Retnq | Retnq_imm16 | Retnw | Retnw_imm16 => true,
        Retfd | Retfd_imm16 | Retfq | Retfq_imm16 | Retfw | Retfw_imm16 => false,
        _ => unreachable!(), // anything else isn't a return instruction.
    }
}

#[cfg(test)]
mod tests {
    use crate::{perf::PerfCollectorConfig, trace_closure, work_loop, TracerBuilder, TracerKind};

    // FIXME: This test won't work until we teach rustc to embed bitcode and emit a basic block
    // section etc.
    #[ignore]
    #[test]
    /// Trace two loops, one 10x larger than the other, then check the proportions match the number
    /// of block the trace passes through.
    fn ten_times_as_many_blocks() {
        let tc = TracerBuilder::new()
            .tracer_kind(TracerKind::PT(PerfCollectorConfig::default()))
            .build()
            .unwrap();

        let trace1 = trace_closure(&tc, || work_loop(10));
        let trace2 = trace_closure(&tc, || work_loop(100));

        let ct1 = trace1.iter_blocks().count();
        let ct2 = trace2.iter_blocks().count();

        // Should be roughly 10x more blocks in trace2. It won't be exactly 10x, due to the stuff
        // we trace either side of the loop itself. On a smallish trace, that will be significant.
        assert!(ct2 > ct1 * 8);
    }
}
