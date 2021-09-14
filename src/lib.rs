/*
 * The following license applies to this file, which derives many
 * details (register and constraint definitions, for example) from the
 * files `BacktrackingAllocator.h`, `BacktrackingAllocator.cpp`,
 * `LIR.h`, and possibly definitions in other related files in
 * `js/src/jit/`:
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 */

#![allow(dead_code)]

pub(crate) mod cfg;
pub(crate) mod domtree;
pub mod indexset;
pub(crate) mod ion;
pub(crate) mod moves;
pub(crate) mod postorder;
pub(crate) mod ssa;

#[macro_use]
mod index;
pub use index::{Block, Inst, InstRange, InstRangeIter};

pub mod checker;

#[cfg(feature = "fuzzing")]
pub mod fuzzing;

/// Register classes.
///
/// Every value has a "register class", which is like a type at the
/// register-allocator level. Every register must belong to only one
/// class; i.e., they are disjoint.
///
/// For tight bit-packing throughout our data structures, we support
/// only two classes, "int" and "float". This will usually be enough
/// on modern machines, as they have one class of general-purpose
/// integer registers of machine width (e.g. 64 bits), and another
/// class of float/vector registers used both for FP and for vector
/// operations. If needed, we could adjust bitpacking to allow for
/// more classes in the future.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RegClass {
    Int = 0,
    Float = 1,
}

/// A physical register. Contains a physical register number and a class.
///
/// The `hw_enc` field contains the physical register number and is in
/// a logically separate index space per class; in other words, Int
/// register 0 is different than Float register 0.
///
/// Because of bit-packed encodings throughout the implementation,
/// `hw_enc` must fit in 6 bits, i.e., at most 64 registers per class.
///
/// The value returned by `index()`, in contrast, is in a single index
/// space shared by all classes, in order to enable uniform reasoning
/// about physical registers. This is done by putting the class bit at
/// the MSB, or equivalently, declaring that indices 0..=63 are the 64
/// integer registers and indices 64..=127 are the 64 float registers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PReg {
    hw_enc: u8,
    class: RegClass,
}

impl PReg {
    pub const MAX_BITS: usize = 6;
    pub const MAX: usize = (1 << Self::MAX_BITS) - 1;
    pub const MAX_INDEX: usize = 1 << (Self::MAX_BITS + 1); // including RegClass bit

    /// Create a new PReg. The `hw_enc` range is 6 bits.
    #[inline(always)]
    pub const fn new(hw_enc: usize, class: RegClass) -> Self {
        // We don't have const panics yet (rust-lang/rust#85194) so we
        // need to use a little indexing trick here. We unfortunately
        // can't use the `static-assertions` crate because we need
        // this to work both for const `hw_enc` and for runtime
        // values.
        const HW_ENC_MUST_BE_IN_BOUNDS: &[bool; PReg::MAX + 1] = &[true; PReg::MAX + 1];
        let _ = HW_ENC_MUST_BE_IN_BOUNDS[hw_enc];

        PReg {
            hw_enc: hw_enc as u8,
            class,
        }
    }

    /// The physical register number, as encoded by the ISA for the particular register class.
    #[inline(always)]
    pub fn hw_enc(self) -> usize {
        let hw_enc = self.hw_enc as usize;
        hw_enc
    }

    /// The register class.
    #[inline(always)]
    pub fn class(self) -> RegClass {
        self.class
    }

    /// Get an index into the (not necessarily contiguous) index space of
    /// all physical registers. Allows one to maintain an array of data for
    /// all PRegs and index it efficiently.
    #[inline(always)]
    pub fn index(self) -> usize {
        ((self.class as u8 as usize) << 5) | (self.hw_enc as usize)
    }

    /// Construct a PReg from the value returned from `.index()`.
    #[inline(always)]
    pub fn from_index(index: usize) -> Self {
        let class = (index >> 5) & 1;
        let class = match class {
            0 => RegClass::Int,
            1 => RegClass::Float,
            _ => unreachable!(),
        };
        let index = index & Self::MAX;
        PReg::new(index, class)
    }

    /// Return the "invalid PReg", which can be used to initialize
    /// data structures.
    #[inline(always)]
    pub fn invalid() -> Self {
        PReg::new(Self::MAX, RegClass::Int)
    }
}

impl std::fmt::Debug for PReg {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "PReg(hw = {}, class = {:?}, index = {})",
            self.hw_enc(),
            self.class(),
            self.index()
        )
    }
}

impl std::fmt::Display for PReg {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let class = match self.class() {
            RegClass::Int => "i",
            RegClass::Float => "f",
        };
        write!(f, "p{}{}", self.hw_enc(), class)
    }
}

/// A virtual register. Contains a virtual register number and a
/// class.
///
/// A virtual register ("vreg") corresponds to an SSA value for SSA
/// input, or just a register when we allow for non-SSA input. All
/// dataflow in the input program is specified via flow through a
/// virtual register; even uses of specially-constrained locations,
/// such as fixed physical registers, are done by using vregs, because
/// we need the vreg's live range in order to track the use of that
/// location.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VReg {
    bits: u32,
}

impl VReg {
    pub const MAX_BITS: usize = 21;
    pub const MAX: usize = (1 << Self::MAX_BITS) - 1;

    #[inline(always)]
    pub const fn new(virt_reg: usize, class: RegClass) -> Self {
        // See comment in `PReg::new()`: we are emulating a const
        // assert here until const panics are stable.
        const VIRT_REG_MUST_BE_IN_BOUNDS: &[bool; VReg::MAX + 1] = &[true; VReg::MAX + 1];
        let _ = VIRT_REG_MUST_BE_IN_BOUNDS[virt_reg];

        VReg {
            bits: ((virt_reg as u32) << 1) | (class as u8 as u32),
        }
    }

    #[inline(always)]
    pub fn vreg(self) -> usize {
        let vreg = (self.bits >> 1) as usize;
        vreg
    }

    #[inline(always)]
    pub fn class(self) -> RegClass {
        match self.bits & 1 {
            0 => RegClass::Int,
            1 => RegClass::Float,
            _ => unreachable!(),
        }
    }

    #[inline(always)]
    pub fn invalid() -> Self {
        VReg::new(Self::MAX, RegClass::Int)
    }
}

impl std::fmt::Debug for VReg {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "VReg(vreg = {}, class = {:?})",
            self.vreg(),
            self.class()
        )
    }
}

impl std::fmt::Display for VReg {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "v{}", self.vreg())
    }
}

/// A spillslot is a space in the stackframe used by the allocator to
/// temporarily store a value.
///
/// The allocator is responsible for allocating indices in this space,
/// and will specify how many spillslots have been used when the
/// allocation is completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpillSlot {
    bits: u32,
}

impl SpillSlot {
    /// Create a new SpillSlot of a given class.
    #[inline(always)]
    pub fn new(slot: usize, class: RegClass) -> Self {
        assert!(slot < (1 << 24));
        SpillSlot {
            bits: (slot as u32) | (class as u8 as u32) << 24,
        }
    }

    /// Get the spillslot index for this spillslot.
    #[inline(always)]
    pub fn index(self) -> usize {
        (self.bits & 0x00ffffff) as usize
    }

    /// Get the class for this spillslot.
    #[inline(always)]
    pub fn class(self) -> RegClass {
        match (self.bits >> 24) as u8 {
            0 => RegClass::Int,
            1 => RegClass::Float,
            _ => unreachable!(),
        }
    }

    /// Get the spillslot `offset` slots away.
    #[inline(always)]
    pub fn plus(self, offset: usize) -> Self {
        SpillSlot::new(self.index() + offset, self.class())
    }

    /// Get the invalid spillslot, used for initializing data structures.
    #[inline(always)]
    pub fn invalid() -> Self {
        SpillSlot { bits: 0xffff_ffff }
    }

    /// Is this the invalid spillslot?
    #[inline(always)]
    pub fn is_invalid(self) -> bool {
        self == Self::invalid()
    }

    /// Is this a valid spillslot (not `SpillSlot::invalid()`)?
    #[inline(always)]
    pub fn is_valid(self) -> bool {
        self != Self::invalid()
    }
}

impl std::fmt::Display for SpillSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "stack{}", self.index())
    }
}

/// An `OperandConstraint` specifies where a vreg's value must be
/// placed at a particular reference to that vreg via an
/// `Operand`. The constraint may be loose -- "any register of a given
/// class", for example -- or very specific, such as "this particular
/// physical register". The allocator's result will always satisfy all
/// given constraints; however, if the input has a combination of
/// constraints that are impossible to satisfy, then allocation may
/// fail or the allocator may panic (providing impossible constraints
/// is usually a programming error in the client, rather than a
/// function of bad input).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperandConstraint {
    /// Any location is fine (register or stack slot).
    Any,
    /// Operand must be in a register. Register is read-only for Uses.
    Reg,
    /// Operand must be on the stack.
    Stack,
    /// Operand must be in a fixed register.
    FixedReg(PReg),
    /// On defs only: reuse a use's register.
    Reuse(usize),
}

impl std::fmt::Display for OperandConstraint {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Any => write!(f, "any"),
            Self::Reg => write!(f, "reg"),
            Self::Stack => write!(f, "stack"),
            Self::FixedReg(preg) => write!(f, "fixed({})", preg),
            Self::Reuse(idx) => write!(f, "reuse({})", idx),
        }
    }
}

/// The "kind" of the operand: whether it reads a vreg (Use), writes a
/// vreg (Def), or reads and then writes (Mod, for "modify").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperandKind {
    Def = 0,
    Mod = 1,
    Use = 2,
}

/// The "position" of the operand: where it has its read/write
/// effects. These are positions "in" the instruction, and "early" and
/// "late" are relative to the instruction's main effect or
/// computation. In other words, the allocator assumes that the
/// instruction (i) performs all reads and writes of "early" operands,
/// (ii) does its work, and (iii) performs all reads and writes of its
/// "late" operands.
///
/// A "write" (def) at "early" or a "read" (use) at "late" may be
/// slightly nonsensical, given the above, if the read is necessary
/// for the computation or the write is a result of it. A way to think
/// of it is that the value (even if a result of execution) *could*
/// have been read or written at the given location without causing
/// any register-usage conflicts. In other words, these write-early or
/// use-late operands ensure that the particular allocations are valid
/// for longer than usual and that a register is not reused between
/// the use (normally complete at "Early") and the def (normally
/// starting at "Late"). See `Operand` for more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperandPos {
    Early = 0,
    Late = 1,
}

/// An `Operand` encodes everything about a mention of a register in
/// an instruction: virtual register number, and any constraint that
/// applies to the register at this program point.
///
/// An Operand may be a use or def (this corresponds to `LUse` and
/// `LAllocation` in Ion).
///
/// Generally, regalloc2 considers operands to have their effects at
/// one of two points that exist in an instruction: "Early" or
/// "Late". All operands at a given program-point are assigned
/// non-conflicting locations based on their constraints. Each operand
/// has a "kind", one of use/def/mod, corresponding to
/// read/write/read-write, respectively.
///
/// Usually, an instruction's inputs will be "early uses" and outputs
/// will be "late defs", though there are valid use-cases for other
/// combinations too. For example, a single "instruction" seen by the
/// regalloc that lowers into multiple machine instructions and reads
/// some of its inputs after it starts to write outputs must either
/// make those input(s) "late uses" or those output(s) "early defs" so
/// that the conflict (overlap) is properly accounted for. See
/// comments on the constructors below for more.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Operand {
    /// Bit-pack into 32 bits.
    ///
    /// constraint:7 kind:2 pos:1 class:1 vreg:21
    ///
    /// where `constraint` is an `OperandConstraint`, `kind` is an
    /// `OperandKind`, `pos` is an `OperandPos`, `class` is a
    /// `RegClass`, and `vreg` is a vreg index.
    ///
    /// The constraints are encoded as follows:
    /// - 1xxxxxx => FixedReg(preg)
    /// - 01xxxxx => Reuse(index)
    /// - 0000000 => Any
    /// - 0000001 => Reg
    /// - 0000010 => Stack
    /// - _ => Unused for now
    bits: u32,
}

impl Operand {
    /// Construct a new operand.
    #[inline(always)]
    pub fn new(
        vreg: VReg,
        constraint: OperandConstraint,
        kind: OperandKind,
        pos: OperandPos,
    ) -> Self {
        let constraint_field = match constraint {
            OperandConstraint::Any => 0,
            OperandConstraint::Reg => 1,
            OperandConstraint::Stack => 2,
            OperandConstraint::FixedReg(preg) => {
                assert_eq!(preg.class(), vreg.class());
                0b1000000 | preg.hw_enc() as u32
            }
            OperandConstraint::Reuse(which) => {
                assert!(which <= 31);
                0b0100000 | which as u32
            }
        };
        let class_field = vreg.class() as u8 as u32;
        let pos_field = pos as u8 as u32;
        let kind_field = kind as u8 as u32;
        Operand {
            bits: vreg.vreg() as u32
                | (class_field << 21)
                | (pos_field << 22)
                | (kind_field << 23)
                | (constraint_field << 25),
        }
    }

    /// Create an `Operand` that designates a use of a VReg that must
    /// be in a register, and that is used at the "before" point,
    /// i.e., can be overwritten by a result.
    #[inline(always)]
    pub fn reg_use(vreg: VReg) -> Self {
        Operand::new(
            vreg,
            OperandConstraint::Reg,
            OperandKind::Use,
            OperandPos::Early,
        )
    }

    /// Create an `Operand` that designates a use of a VReg that must
    /// be in a register, and that is used up until the "after" point,
    /// i.e., must not conflict with any results.
    #[inline(always)]
    pub fn reg_use_at_end(vreg: VReg) -> Self {
        Operand::new(
            vreg,
            OperandConstraint::Reg,
            OperandKind::Use,
            OperandPos::Late,
        )
    }

    /// Create an `Operand` that designates a definition of a VReg
    /// that must be in a register, and that occurs at the "after"
    /// point, i.e. may reuse a register that carried a use into this
    /// instruction.
    #[inline(always)]
    pub fn reg_def(vreg: VReg) -> Self {
        Operand::new(
            vreg,
            OperandConstraint::Reg,
            OperandKind::Def,
            OperandPos::Late,
        )
    }

    /// Create an `Operand` that designates a definition of a VReg
    /// that must be in a register, and that occurs early at the
    /// "before" point, i.e., must not conflict with any input to the
    /// instruction.
    #[inline(always)]
    pub fn reg_def_at_start(vreg: VReg) -> Self {
        Operand::new(
            vreg,
            OperandConstraint::Reg,
            OperandKind::Def,
            OperandPos::Early,
        )
    }

    /// Create an `Operand` that designates a def (and use) of a
    /// temporary *within* the instruction. This register is assumed
    /// to be written by the instruction, and will not conflict with
    /// any input or output, but should not be used after the
    /// instruction completes.
    ///
    /// Note that within a single instruction, the dedicated scratch
    /// register (as specified in the `MachineEnv`) is also always
    /// available for use. The register allocator may use the register
    /// *between* instructions in order to implement certain sequences
    /// of moves, but will never hold a value live in the scratch
    /// register across an instruction.
    #[inline(always)]
    pub fn reg_temp(vreg: VReg) -> Self {
        // For now a temp is equivalent to a def-at-start operand,
        // which gives the desired semantics but does not enforce the
        // "not reused later" constraint.
        Operand::new(
            vreg,
            OperandConstraint::Reg,
            OperandKind::Def,
            OperandPos::Early,
        )
    }

    /// Create an `Operand` that designates a def of a vreg that must
    /// reuse the register assigned to an input to the
    /// instruction. The input is identified by `idx` (is the `idx`th
    /// `Operand` for the instruction) and must be constraint to a
    /// register, i.e., be the result of `Operand::reg_use(vreg)`.
    #[inline(always)]
    pub fn reg_reuse_def(vreg: VReg, idx: usize) -> Self {
        Operand::new(
            vreg,
            OperandConstraint::Reuse(idx),
            OperandKind::Def,
            OperandPos::Late,
        )
    }

    /// Create an `Operand` that designates a use of a vreg and
    /// ensures that it is placed in the given, fixed PReg at the
    /// use. It is guaranteed that the `Allocation` resulting for this
    /// operand will be `preg`.
    #[inline(always)]
    pub fn reg_fixed_use(vreg: VReg, preg: PReg) -> Self {
        Operand::new(
            vreg,
            OperandConstraint::FixedReg(preg),
            OperandKind::Use,
            OperandPos::Early,
        )
    }

    /// Create an `Operand` that designates a def of a vreg and
    /// ensures that it is placed in the given, fixed PReg at the
    /// def. It is guaranteed that the `Allocation` resulting for this
    /// operand will be `preg`.
    #[inline(always)]
    pub fn reg_fixed_def(vreg: VReg, preg: PReg) -> Self {
        Operand::new(
            vreg,
            OperandConstraint::FixedReg(preg),
            OperandKind::Def,
            OperandPos::Late,
        )
    }

    /// Get the virtual register designated by an operand. Every
    /// operand must name some virtual register, even if it constrains
    /// the operand to a fixed physical register as well; the vregs
    /// are used to track dataflow.
    #[inline(always)]
    pub fn vreg(self) -> VReg {
        let vreg_idx = ((self.bits as usize) & VReg::MAX) as usize;
        VReg::new(vreg_idx, self.class())
    }

    /// Get the register class used by this operand.
    #[inline(always)]
    pub fn class(self) -> RegClass {
        let class_field = (self.bits >> 21) & 1;
        match class_field {
            0 => RegClass::Int,
            1 => RegClass::Float,
            _ => unreachable!(),
        }
    }

    /// Get the "kind" of this operand: a definition (write), a use
    /// (read), or a "mod" / modify (a read followed by a write).
    #[inline(always)]
    pub fn kind(self) -> OperandKind {
        let kind_field = (self.bits >> 23) & 3;
        match kind_field {
            0 => OperandKind::Def,
            1 => OperandKind::Mod,
            2 => OperandKind::Use,
            _ => unreachable!(),
        }
    }

    /// Get the "position" of this operand, i.e., where its read
    /// and/or write occurs: either before the instruction executes,
    /// or after it does. Ordinarily, uses occur at "before" and defs
    /// at "after", though there are cases where this is not true.
    #[inline(always)]
    pub fn pos(self) -> OperandPos {
        let pos_field = (self.bits >> 22) & 1;
        match pos_field {
            0 => OperandPos::Early,
            1 => OperandPos::Late,
            _ => unreachable!(),
        }
    }

    /// Get the "constraint" of this operand, i.e., what requirements
    /// its allocation must fulfill.
    #[inline(always)]
    pub fn constraint(self) -> OperandConstraint {
        let constraint_field = ((self.bits >> 25) as usize) & 127;
        if constraint_field & 0b1000000 != 0 {
            OperandConstraint::FixedReg(PReg::new(constraint_field & 0b0111111, self.class()))
        } else if constraint_field & 0b0100000 != 0 {
            OperandConstraint::Reuse(constraint_field & 0b0011111)
        } else {
            match constraint_field {
                0 => OperandConstraint::Any,
                1 => OperandConstraint::Reg,
                2 => OperandConstraint::Stack,
                _ => unreachable!(),
            }
        }
    }

    /// Get the raw 32-bit encoding of this operand's fields.
    #[inline(always)]
    pub fn bits(self) -> u32 {
        self.bits
    }

    /// Construct an `Operand` from the raw 32-bit encoding returned
    /// from `bits()`.
    #[inline(always)]
    pub fn from_bits(bits: u32) -> Self {
        debug_assert!(bits >> 29 <= 4);
        Operand { bits }
    }
}

impl std::fmt::Debug for Operand {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::fmt::Display for Operand {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match (self.kind(), self.pos()) {
            (OperandKind::Def, OperandPos::Late)
            | (OperandKind::Mod | OperandKind::Use, OperandPos::Early) => {
                write!(f, "{:?}", self.kind())?;
            }
            _ => {
                write!(f, "{:?}@{:?}", self.kind(), self.pos())?;
            }
        }
        write!(
            f,
            ": {}{} {}",
            self.vreg(),
            match self.class() {
                RegClass::Int => "i",
                RegClass::Float => "f",
            },
            self.constraint()
        )
    }
}

/// An Allocation represents the end result of regalloc for an
/// Operand.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Allocation {
    /// Bit-pack in 32 bits.
    ///
    /// kind:3 unused:1 index:28
    bits: u32,
}

impl std::fmt::Debug for Allocation {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::fmt::Display for Allocation {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.kind() {
            AllocationKind::None => write!(f, "none"),
            AllocationKind::Reg => write!(f, "{}", self.as_reg().unwrap()),
            AllocationKind::Stack => write!(f, "{}", self.as_stack().unwrap()),
        }
    }
}

impl Allocation {
    /// Construct a new Allocation.
    #[inline(always)]
    pub(crate) fn new(kind: AllocationKind, index: usize) -> Self {
        assert!(index < (1 << 28));
        Self {
            bits: ((kind as u8 as u32) << 29) | (index as u32),
        }
    }

    /// Get the "none" allocation, which is distinct from the other
    /// possibilities and is used to initialize data structures.
    #[inline(always)]
    pub fn none() -> Allocation {
        Allocation::new(AllocationKind::None, 0)
    }

    /// Create an allocation into a register.
    #[inline(always)]
    pub fn reg(preg: PReg) -> Allocation {
        Allocation::new(AllocationKind::Reg, preg.index())
    }

    /// Create an allocation into a spillslot.
    #[inline(always)]
    pub fn stack(slot: SpillSlot) -> Allocation {
        Allocation::new(AllocationKind::Stack, slot.bits as usize)
    }

    /// Get the allocation's "kind": none, register, or stack (spillslot).
    #[inline(always)]
    pub fn kind(self) -> AllocationKind {
        match (self.bits >> 29) & 7 {
            0 => AllocationKind::None,
            1 => AllocationKind::Reg,
            2 => AllocationKind::Stack,
            _ => unreachable!(),
        }
    }

    /// Is the allocation "none"?
    #[inline(always)]
    pub fn is_none(self) -> bool {
        self.kind() == AllocationKind::None
    }

    /// Is the allocation not "none"?
    #[inline(always)]
    pub fn is_some(self) -> bool {
        self.kind() != AllocationKind::None
    }

    /// Is the allocation a register?
    #[inline(always)]
    pub fn is_reg(self) -> bool {
        self.kind() == AllocationKind::Reg
    }

    /// Is the allocation on the stack (a spillslot)?
    #[inline(always)]
    pub fn is_stack(self) -> bool {
        self.kind() == AllocationKind::Stack
    }

    /// Get the index of the spillslot or register. If register, this
    /// is an index that can be used by `PReg::from_index()`.
    #[inline(always)]
    pub fn index(self) -> usize {
        (self.bits & ((1 << 28) - 1)) as usize
    }

    /// Get the allocation as a physical register, if any.
    #[inline(always)]
    pub fn as_reg(self) -> Option<PReg> {
        if self.kind() == AllocationKind::Reg {
            Some(PReg::from_index(self.index()))
        } else {
            None
        }
    }

    /// Get the allocation as a spillslot, if any.
    #[inline(always)]
    pub fn as_stack(self) -> Option<SpillSlot> {
        if self.kind() == AllocationKind::Stack {
            Some(SpillSlot {
                bits: self.index() as u32,
            })
        } else {
            None
        }
    }

    /// Get the raw bits for the packed encoding of this allocation.
    #[inline(always)]
    pub fn bits(self) -> u32 {
        self.bits
    }

    /// Construct an allocation from its packed encoding.
    #[inline(always)]
    pub fn from_bits(bits: u32) -> Self {
        debug_assert!(bits >> 29 >= 5);
        Self { bits }
    }
}

/// An allocation is one of two "kinds" (or "none"): register or
/// spillslot/stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum AllocationKind {
    None = 0,
    Reg = 1,
    Stack = 2,
}

impl Allocation {
    /// Get the register class of an allocation's value.
    #[inline(always)]
    pub fn class(self) -> RegClass {
        match self.kind() {
            AllocationKind::None => panic!("Allocation::None has no class"),
            AllocationKind::Reg => self.as_reg().unwrap().class(),
            AllocationKind::Stack => self.as_stack().unwrap().class(),
        }
    }
}

/// A trait defined by the regalloc client to provide access to its
/// machine-instruction / CFG representation.
///
/// (This trait's design is inspired by, and derives heavily from, the
/// trait of the same name in regalloc.rs.)
pub trait Function {
    // -------------
    // CFG traversal
    // -------------

    /// How many instructions are there?
    fn num_insts(&self) -> usize;

    /// How many blocks are there?
    fn num_blocks(&self) -> usize;

    /// Get the index of the entry block.
    fn entry_block(&self) -> Block;

    /// Provide the range of instruction indices contained in each block.
    fn block_insns(&self, block: Block) -> InstRange;

    /// Get CFG successors for a given block.
    fn block_succs(&self, block: Block) -> &[Block];

    /// Get the CFG predecessors for a given block.
    fn block_preds(&self, block: Block) -> &[Block];

    /// Get the block parameters for a given block.
    fn block_params(&self, block: Block) -> &[VReg];

    /// Determine whether an instruction is a return instruction.
    fn is_ret(&self, insn: Inst) -> bool;

    /// Determine whether an instruction is the end-of-block
    /// branch. If so, its operands at the indices given by
    /// `branch_blockparam_arg_offset()` below *must* be the block
    /// parameters for each of its block's `block_succs` successor
    /// blocks, in order.
    fn is_branch(&self, insn: Inst) -> bool;

    /// If `insn` is a branch at the end of `block`, returns the
    /// operand index at which outgoing blockparam arguments are
    /// found. Starting at this index, blockparam arguments for each
    /// successor block's blockparams, in order, must be found.
    ///
    /// It is an error if `self.inst_operands(insn).len() -
    /// self.branch_blockparam_arg_offset(insn)` is not exactly equal
    /// to the sum of blockparam counts for all successor blocks.
    fn branch_blockparam_arg_offset(&self, block: Block, insn: Inst) -> usize;

    /// Determine whether an instruction requires all reference-typed
    /// values to be placed onto the stack. For these instructions,
    /// stackmaps will be provided.
    ///
    /// This is usually associated with the concept of a "safepoint",
    /// though strictly speaking, a safepoint could also support
    /// reference-typed values in registers if there were a way to
    /// denote their locations and if this were acceptable to the
    /// client. Usually garbage-collector implementations want to see
    /// roots on the stack, so we do that for now.
    fn requires_refs_on_stack(&self, _: Inst) -> bool {
        false
    }

    /// Determine whether an instruction is a move; if so, return the
    /// Operands for (src, dst).
    fn is_move(&self, insn: Inst) -> Option<(Operand, Operand)>;

    // --------------------------
    // Instruction register slots
    // --------------------------

    /// Get the Operands for an instruction.
    fn inst_operands(&self, insn: Inst) -> &[Operand];

    /// Get the clobbers for an instruction; these are the registers
    /// that, after the instruction has executed, hold values that are
    /// arbitrary, separately from the usual outputs to the
    /// instruction. It is invalid to read a register that has been
    /// clobbered; the register allocator is free to assume that
    /// clobbered registers are filled with garbage and available for
    /// reuse. It will avoid storing any value in a clobbered register
    /// that must be live across the instruction.
    ///
    /// Another way of seeing this is that a clobber is equivalent to
    /// an "early def" of a fresh vreg that is not used anywhere else
    /// in the program, with a fixed-register constraint that places
    /// it in a given PReg chosen by the client prior to regalloc.
    ///
    /// Every register written by an instruction must either
    /// correspond to (be assigned to) an Operand of kind `Def` or
    /// `Mod`, or else must be a "clobber".
    ///
    /// This can be used to, for example, describe ABI-specified
    /// registers that are not preserved by a call instruction, or
    /// fixed physical registers written by an instruction but not
    /// used as a vreg output, or fixed physical registers used as
    /// temps within an instruction out of necessity.
    fn inst_clobbers(&self, insn: Inst) -> &[PReg];

    /// Get the number of `VReg` in use in this function.
    fn num_vregs(&self) -> usize;

    /// Get the VRegs that are pointer/reference types. This has the
    /// following effects for each such vreg:
    ///
    /// - At all safepoint instructions, the vreg will be in a
    ///   SpillSlot, not in a register.
    /// - The vreg *may not* be used as a register operand on
    ///   safepoint instructions: this is because a vreg can only live
    ///   in one place at a time. The client should copy the value to an
    ///   integer-typed vreg and use this to pass a pointer as an input
    ///   to a safepoint instruction (such as a function call).
    /// - At all safepoint instructions, all live vregs' locations
    ///   will be included in a list in the `Output` below, so that
    ///   pointer-inspecting/updating functionality (such as a moving
    ///   garbage collector) may observe and edit their values.
    fn reftype_vregs(&self) -> &[VReg] {
        &[]
    }

    /// Get the VRegs for which we should generate value-location
    /// metadata for debugging purposes. This can be used to generate
    /// e.g. DWARF with valid prgram-point ranges for each value
    /// expression in a way that is more efficient than a post-hoc
    /// analysis of the allocator's output.
    ///
    /// Each tuple is (vreg, inclusive_start, exclusive_end,
    /// label). In the `Output` there will be (label, inclusive_start,
    /// exclusive_end, alloc)` tuples. The ranges may not exactly
    /// match -- specifically, the returned metadata may cover only a
    /// subset of the requested ranges -- if the value is not live for
    /// the entire requested ranges.
    fn debug_value_labels(&self) -> &[(Inst, Inst, VReg, u32)] {
        &[]
    }

    /// Is the given vreg pinned to a preg? If so, every use of the
    /// vreg is automatically assigned to the preg, and live-ranges of
    /// the vreg allocate the preg exclusively (are not spilled
    /// elsewhere). The user must take care not to have too many live
    /// pinned vregs such that allocation is no longer possible;
    /// liverange computation will check that this is the case (that
    /// there are enough remaining allocatable pregs of every class to
    /// hold all Reg-constrained operands).
    fn is_pinned_vreg(&self, _: VReg) -> Option<PReg> {
        None
    }

    /// Return a list of all pinned vregs.
    fn pinned_vregs(&self) -> &[VReg] {
        &[]
    }

    // --------------
    // Spills/reloads
    // --------------

    /// How many logical spill slots does the given regclass require?  E.g., on
    /// a 64-bit machine, spill slots may nominally be 64-bit words, but a
    /// 128-bit vector value will require two slots.  The regalloc will always
    /// align on this size.
    ///
    /// (This trait method's design and doc text derives from
    /// regalloc.rs' trait of the same name.)
    fn spillslot_size(&self, regclass: RegClass) -> usize;

    /// When providing a spillslot number for a multi-slot spillslot,
    /// do we provide the first or the last? This is usually related
    /// to which direction the stack grows and different clients may
    /// have different preferences.
    fn multi_spillslot_named_by_last_slot(&self) -> bool {
        false
    }
}

/// A position before or after an instruction at which we can make an
/// edit.
///
/// Note that this differs from `OperandPos` in that the former
/// describes specifically a constraint on an operand, while this
/// describes a program point. `OperandPos` could grow more options in
/// the future, for example if we decide that an "early write" or
/// "late read" phase makes sense, while `InstPosition` will always
/// describe these two insertion points.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum InstPosition {
    Before = 0,
    After = 1,
}

/// A program point: a single point before or after a given instruction.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProgPoint {
    bits: u32,
}

impl std::fmt::Debug for ProgPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "progpoint{}{}",
            self.inst().index(),
            match self.pos() {
                InstPosition::Before => "-pre",
                InstPosition::After => "-post",
            }
        )
    }
}

impl ProgPoint {
    /// Create a new ProgPoint before or after the given instruction.
    #[inline(always)]
    pub fn new(inst: Inst, pos: InstPosition) -> Self {
        let bits = ((inst.0 as u32) << 1) | (pos as u8 as u32);
        Self { bits }
    }

    /// Create a new ProgPoint before the given instruction.
    #[inline(always)]
    pub fn before(inst: Inst) -> Self {
        Self::new(inst, InstPosition::Before)
    }

    /// Create a new ProgPoint after the given instruction.
    #[inline(always)]
    pub fn after(inst: Inst) -> Self {
        Self::new(inst, InstPosition::After)
    }

    /// Get the instruction that this ProgPoint is before or after.
    #[inline(always)]
    pub fn inst(self) -> Inst {
        // Cast to i32 to do an arithmetic right-shift, which will
        // preserve an `Inst::invalid()` (which is -1, or all-ones).
        Inst::new(((self.bits as i32) >> 1) as usize)
    }

    /// Get the "position" (Before or After) relative to the
    /// instruction.
    #[inline(always)]
    pub fn pos(self) -> InstPosition {
        match self.bits & 1 {
            0 => InstPosition::Before,
            1 => InstPosition::After,
            _ => unreachable!(),
        }
    }

    /// Get the "next" program point: for After, this is the Before of
    /// the next instruction, while for Before, this is After of the
    /// same instruction.
    #[inline(always)]
    pub fn next(self) -> ProgPoint {
        Self {
            bits: self.bits + 1,
        }
    }

    /// Get the "previous" program point, the inverse of `.next()`
    /// above.
    #[inline(always)]
    pub fn prev(self) -> ProgPoint {
        Self {
            bits: self.bits - 1,
        }
    }

    /// Convert to a raw encoding in 32 bits.
    #[inline(always)]
    pub fn to_index(self) -> u32 {
        self.bits
    }

    /// Construct from the raw 32-bit encoding.
    #[inline(always)]
    pub fn from_index(index: u32) -> Self {
        Self { bits: index }
    }
}

/// An instruction to insert into the program to perform some data movement.
#[derive(Clone, Debug)]
pub enum Edit {
    /// Move one allocation to another. Each allocation may be a
    /// register or a stack slot (spillslot). However, stack-to-stack
    /// moves will never be generated.
    ///
    /// `to_vreg`, if defined, is useful as metadata: it indicates
    /// that the moved value is a def of a new vreg.
    ///
    /// `Move` edits will be generated even if src and dst allocation
    /// are the same if the vreg changes; this allows proper metadata
    /// tracking even when moves are elided.
    Move {
        from: Allocation,
        to: Allocation,
        to_vreg: Option<VReg>,
    },

    /// Define a particular Allocation to contain a particular VReg. Useful
    /// for the checker.
    DefAlloc { alloc: Allocation, vreg: VReg },
}

/// A machine envrionment tells the register allocator which registers
/// are available to allocate and what register may be used as a
/// scratch register for each class, and some other miscellaneous info
/// as well.
#[derive(Clone, Debug)]
pub struct MachineEnv {
    /// Physical registers. Every register that might be mentioned in
    /// any constraint must be listed here, even if it is not
    /// allocatable (present in one of
    /// `{preferred,non_preferred}_regs_by_class`).
    pub regs: Vec<PReg>,

    /// Preferred physical registers for each class. These are the
    /// registers that will be allocated first, if free.
    pub preferred_regs_by_class: [Vec<PReg>; 2],

    /// Non-preferred physical registers for each class. These are the
    /// registers that will be allocated if a preferred register is
    /// not available; using one of these is considered suboptimal,
    /// but still better than spilling.
    pub non_preferred_regs_by_class: [Vec<PReg>; 2],

    /// One scratch register per class. This is needed to perform
    /// moves between registers when cyclic move patterns occur. The
    /// register should not be placed in either the preferred or
    /// non-preferred list (i.e., it is not otherwise allocatable).
    ///
    /// Note that the register allocator will freely use this register
    /// between instructions, but *within* the machine code generated
    /// by a single (regalloc-level) instruction, the client is free
    /// to use the scratch register. E.g., if one "instruction" causes
    /// the emission of two machine-code instructions, this lowering
    /// can use the scratch register between them.
    pub scratch_by_class: [PReg; 2],
}

/// The output of the register allocator.
#[derive(Clone, Debug)]
pub struct Output {
    /// How many spillslots are needed in the frame?
    pub num_spillslots: usize,

    /// Edits (insertions or removals). Guaranteed to be sorted by
    /// program point.
    pub edits: Vec<(ProgPoint, Edit)>,

    /// Allocations for each operand. Mapping from instruction to
    /// allocations provided by `inst_alloc_offsets` below.
    pub allocs: Vec<Allocation>,

    /// Allocation offset in `allocs` for each instruction.
    pub inst_alloc_offsets: Vec<u32>,

    /// Safepoint records: at a given program point, a reference-typed value lives in the given SpillSlot.
    pub safepoint_slots: Vec<(ProgPoint, SpillSlot)>,

    /// Debug info: a labeled value (as applied to vregs by
    /// `Function::debug_value_labels()` on the input side) is located
    /// in the given allocation from the first program point
    /// (inclusive) to the second (exclusive). Guaranteed to be sorted
    /// by label and program point, and the ranges are guaranteed to
    /// be disjoint.
    pub debug_locations: Vec<(u32, ProgPoint, ProgPoint, Allocation)>,

    /// Internal stats from the allocator.
    pub stats: ion::Stats,
}

impl Output {
    /// Get the allocations assigned to a given instruction.
    pub fn inst_allocs(&self, inst: Inst) -> &[Allocation] {
        let start = self.inst_alloc_offsets[inst.index()] as usize;
        let end = if inst.index() + 1 == self.inst_alloc_offsets.len() {
            self.allocs.len()
        } else {
            self.inst_alloc_offsets[inst.index() + 1] as usize
        };
        &self.allocs[start..end]
    }
}

/// An error that prevents allocation.
#[derive(Clone, Debug)]
pub enum RegAllocError {
    /// Critical edge is not split between given blocks.
    CritEdge(Block, Block),
    /// Invalid SSA for given vreg at given inst: multiple defs or
    /// illegal use. `inst` may be `Inst::invalid()` if this concerns
    /// a block param.
    SSA(VReg, Inst),
    /// Invalid basic block: does not end in branch/ret, or contains a
    /// branch/ret in the middle.
    BB(Block),
    /// Invalid branch: operand count does not match sum of block
    /// params of successor blocks.
    Branch(Inst),
    /// A VReg is live-in on entry; this is not allowed.
    EntryLivein,
    /// A branch has non-blockparam arg(s) and at least one of the
    /// successor blocks has more than one predecessor, forcing
    /// edge-moves before this branch. This is disallowed because it
    /// places a use after the edge moves occur; insert an edge block
    /// to avoid the situation.
    DisallowedBranchArg(Inst),
    /// Too many pinned VRegs + Reg-constrained Operands are live at
    /// once, making allocation impossible.
    TooManyLiveRegs,
}

impl std::fmt::Display for RegAllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for RegAllocError {}

/// Run the allocator.
pub fn run<F: Function>(
    func: &F,
    env: &MachineEnv,
    options: &RegallocOptions,
) -> Result<Output, RegAllocError> {
    ion::run(func, env, options.verbose_log)
}

/// Options for allocation.
#[derive(Clone, Copy, Debug, Default)]
pub struct RegallocOptions {
    /// Add extra verbosity to debug logs.
    pub verbose_log: bool,
}
