use std::fmt;

type BlockAddr = usize;

/// Information about a trace decoder's notion of a basic block.
///
/// The exact definition of a basic block will vary from collector to collector.
#[derive(Eq, PartialEq)]
pub enum Block {
    /// An address range that captures at least the first byte of every machine instruction in the
    /// block.
    VAddrRange {
        /// Virtual address of the start of the first instruction in this block.
        first_inst: BlockAddr,
        /// Virtual address of *any* byte of the last instruction in this block.
        last_inst: BlockAddr,
    },
    /// An unknown virtual address range.
    ///
    /// This is required because decoders don't have perfect knowledge about every block
    /// in the virtual address space.
    Unknown,
}

impl fmt::Debug for Block {
    /// Format virtual addresses using hexadecimal.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::VAddrRange {
                first_inst,
                last_inst,
            } => {
                write!(f, "Block({first_inst:x}..={last_inst:x})")
            }
            Self::Unknown => {
                write!(f, "UnknownBlock")
            }
        }
    }
}

impl Block {
    /// Creates a new basic block from the virtual addresses of:
    ///   * the start of the first instruction in the basic block.
    ///   * the start of the last instruction in the basic block.
    pub fn from_vaddr_range(first_inst: BlockAddr, last_inst: BlockAddr) -> Self {
        Self::VAddrRange {
            first_inst,
            last_inst,
        }
    }

    /// Returns `true` if `self` represents an unknown virtual address range.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }

    /// If `self` represents a known address range, returns the address range, otherwise `None`.
    pub fn vaddr_range(&self) -> Option<(BlockAddr, BlockAddr)> {
        if let Self::VAddrRange {
            first_inst,
            last_inst,
        } = self
        {
            Some((*first_inst, *last_inst))
        } else {
            None
        }
    }
}
