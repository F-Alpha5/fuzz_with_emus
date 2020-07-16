//! A 64-bit RISC-V RV64i interpreter

use std::fmt;
use std::sync::Arc;
use std::process::Command;
use std::collections::BTreeMap;
use crate::Corpus;
use crate::mmu::{VirtAddr, Perm, PERM_READ, PERM_WRITE, PERM_EXEC};
use crate::mmu::{Mmu, DIRTY_BLOCK_SIZE};
use crate::jitcache::JitCache;

/// An R-type instruction
#[derive(Debug)]
struct Rtype {
    funct7: u32,
    rs2:    Register,
    rs1:    Register,
    funct3: u32,
    rd:     Register,
}

impl From<u32> for Rtype {
    fn from(inst: u32) -> Self {
        Rtype {
            funct7: (inst >> 25) & 0b1111111,
            rs2:    Register::from((inst >> 20) & 0b11111),
            rs1:    Register::from((inst >> 15) & 0b11111),
            funct3: (inst >> 12) & 0b111,
            rd:     Register::from((inst >>  7) & 0b11111),
        }
    }
}

/// An S-type instruction
#[derive(Debug)]
struct Stype {
    imm:    i32,
    rs2:    Register,
    rs1:    Register,
    funct3: u32,
}

impl From<u32> for Stype {
    fn from(inst: u32) -> Self {
        let imm115 = (inst >> 25) & 0b1111111;
        let imm40  = (inst >>  7) & 0b11111;

        let imm = (imm115 << 5) | imm40;
        let imm = ((imm as i32) << 20) >> 20;

        Stype {
            imm:    imm,
            rs2:    Register::from((inst >> 20) & 0b11111),
            rs1:    Register::from((inst >> 15) & 0b11111),
            funct3: (inst >> 12) & 0b111,
        }
    }
}

/// A J-type instruction
#[derive(Debug)]
struct Jtype {
    imm: i32,
    rd:  Register,
}

impl From<u32> for Jtype {
    fn from(inst: u32) -> Self {
        let imm20   = (inst >> 31) & 1;
        let imm101  = (inst >> 21) & 0b1111111111;
        let imm11   = (inst >> 20) & 1;
        let imm1912 = (inst >> 12) & 0b11111111;

        let imm = (imm20 << 20) | (imm1912 << 12) | (imm11 << 11) |
            (imm101 << 1);
        let imm = ((imm as i32) << 11) >> 11;

        Jtype {
            imm: imm,
            rd:  Register::from((inst >> 7) & 0b11111),
        }
    }
}

/// A B-type instruction
#[derive(Debug)]
struct Btype {
    imm:    i32,
    rs2:    Register,
    rs1:    Register,
    funct3: u32,
}

impl From<u32> for Btype {
    fn from(inst: u32) -> Self {
        let imm12  = (inst >> 31) & 1;
        let imm105 = (inst >> 25) & 0b111111;
        let imm41  = (inst >>  8) & 0b1111;
        let imm11  = (inst >>  7) & 1;

        let imm = (imm12 << 12) | (imm11 << 11) |(imm105 << 5) | (imm41 << 1);
        let imm = ((imm as i32) << 19) >> 19;

        Btype {
            imm:    imm,
            rs2:    Register::from((inst >> 20) & 0b11111),
            rs1:    Register::from((inst >> 15) & 0b11111),
            funct3: (inst >> 12) & 0b111,
        }
    }
}

/// An I-type instruction
#[derive(Debug)]
struct Itype {
    imm:    i32,
    rs1:    Register,
    funct3: u32,
    rd:     Register,
}

impl From<u32> for Itype {
    fn from(inst: u32) -> Self {
        let imm = (inst as i32) >> 20;
        Itype {
            imm:    imm,
            rs1:    Register::from((inst >> 15) & 0b11111),
            funct3: (inst >> 12) & 0b111,
            rd:     Register::from((inst >>  7) & 0b11111),
        }
    }
}

#[derive(Debug)]
struct Utype {
    imm: i32,
    rd:  Register,
}

impl From<u32> for Utype {
    fn from(inst: u32) -> Self {
        Utype {
            imm: (inst & !0xfff) as i32,
            rd:  Register::from((inst >> 7) & 0b11111),
        }
    }
}

/// An open file
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmuFile {
    Stdin,
    Stdout,
    Stderr,

    // A file which is backed by the current fuzz input
    FuzzInput { cursor: usize },
}

/// A list of all open files
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Files(Vec<Option<EmuFile>>);

impl Files {
    /// Get access to a file descriptor for `fd`
    pub fn get_file(&mut self, fd: usize) -> Option<&mut Option<EmuFile>> {
        self.0.get_mut(fd)
    }
}

/// Callback for breakpoints
type BreakpointCallback = fn(&mut Emulator) -> Result<(), VmExit>;

/// All the state of the emulated system
pub struct Emulator {
    /// Memory for the emulator
    pub memory: Mmu,

    /// All RV64i registers
    registers: [u64; 33],

    /// Fuzz input for the program
    pub fuzz_input: Vec<u8>,

    /// File handle table (indexed by file descriptor)
    pub files: Files,

    /// Breakpoint callbacks
    breakpoints: BTreeMap<VirtAddr, BreakpointCallback>,

    /// JIT cache, if we are using a JIT
    jit_cache: Option<Arc<JitCache>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Reasons why the VM exited
pub enum VmExit {
    /// The VM exited due to a syscall instruction
    Syscall,

    /// The VM exited cleanly as requested by the VM
    Exit,

    /// The instruction count limit was hit and a timeout has occurred
    Timeout,

    /// An invalid opcode was lifted
    InvalidOpcode,

    /// A free of an invalid region was performed
    InvalidFree(VirtAddr),

    /// An integer overflow occured during a syscall due to bad supplied
    /// arguments by the program
    SyscallIntegerOverflow,

    /// A read or write memory request overflowed the address size
    AddressIntegerOverflow,

    /// The address requested was not in bounds of the guest memory space
    AddressMiss(VirtAddr, usize),

    /// An read of `VirtAddr` failed due to missing permissions
    ReadFault(VirtAddr),

    /// An execution of a `VirtAddr` failed
    ExecFault(VirtAddr),

    /// A read of memory which is uninitialized, but otherwise readable failed
    /// at `VirtAddr`
    UninitFault(VirtAddr),
    
    /// An write of `VirtAddr` failed due to missing permissions
    WriteFault(VirtAddr),
}

/// Different types of faults
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultType {
    // Access occurred outside of program memory
    Bounds,

    // Invalid free (eg, double free or corrupt free address)
    Free,

    // An invalid opcode was executed (or lifted)
    InvalidOpcode,

    Exec,
    Read,
    Write,
    Uninit,
}

/// Different buckets for addresses
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressType {
    /// Address was between [0, 32 KiB)
    Null,

    /// Address was between [-32 KiB, 0)
    Negative,

    /// Address was normal
    Normal,
}

impl From<VirtAddr> for AddressType {
    fn from(val: VirtAddr) -> Self {
        match val.0 as i64 {
            (0..=32767)   => AddressType::Null,
            (-32768..=-1) => AddressType::Negative,
            _ => AddressType::Normal,
        }
    }
}

impl VmExit {
    /// If this is a crash it returns the faulting address and the fault type
    pub fn is_crash(&self) -> Option<(FaultType, VirtAddr)> {
        match *self {
            VmExit::AddressMiss(addr, _) => Some((FaultType::Bounds, addr)),
            VmExit::ReadFault(addr)      => Some((FaultType::Read,   addr)),
            VmExit::ExecFault(addr)      => Some((FaultType::Exec,   addr)),
            VmExit::UninitFault(addr)    => Some((FaultType::Uninit, addr)),
            VmExit::WriteFault(addr)     => Some((FaultType::Write,  addr)),
            VmExit::InvalidFree(addr)    => Some((FaultType::Free,   addr)),
            VmExit::InvalidOpcode =>
                Some((FaultType::InvalidOpcode, VirtAddr(0))),
            _ => None,
        }
    }
}

impl fmt::Display for Emulator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f,
r#"zero {:016x} ra {:016x} sp  {:016x} gp  {:016x}
tp   {:016x} t0 {:016x} t1  {:016x} t2  {:016x}
s0   {:016x} s1 {:016x} a0  {:016x} a1  {:016x}
a2   {:016x} a3 {:016x} a4  {:016x} a5  {:016x}
a6   {:016x} a7 {:016x} s2  {:016x} s3  {:016x}
s4   {:016x} s5 {:016x} s6  {:016x} s7  {:016x}
s8   {:016x} s9 {:016x} s10 {:016x} s11 {:016x}
t3   {:016x} t4 {:016x} t5  {:016x} t6  {:016x}
pc   {:016x}"#,
        self.reg(Register::Zero),
        self.reg(Register::Ra),
        self.reg(Register::Sp),
        self.reg(Register::Gp),
        self.reg(Register::Tp),
        self.reg(Register::T0),
        self.reg(Register::T1),
        self.reg(Register::T2),
        self.reg(Register::S0),
        self.reg(Register::S1),
        self.reg(Register::A0),
        self.reg(Register::A1),
        self.reg(Register::A2),
        self.reg(Register::A3),
        self.reg(Register::A4),
        self.reg(Register::A5),
        self.reg(Register::A6),
        self.reg(Register::A7),
        self.reg(Register::S2),
        self.reg(Register::S3),
        self.reg(Register::S4),
        self.reg(Register::S5),
        self.reg(Register::S6),
        self.reg(Register::S7),
        self.reg(Register::S8),
        self.reg(Register::S9),
        self.reg(Register::S10),
        self.reg(Register::S11),
        self.reg(Register::T3),
        self.reg(Register::T4),
        self.reg(Register::T5),
        self.reg(Register::T6),
        self.reg(Register::Pc))
    }
}

/// 64-bit RISC-V registers
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Register {
    Zero = 0,
    Ra,
    Sp,
    Gp,
    Tp,
    T0,
    T1,
    T2,
    S0,
    S1,
    A0,
    A1,
    A2,
    A3,
    A4,
    A5,
    A6,
    A7,
    S2,
    S3,
    S4,
    S5,
    S6,
    S7,
    S8,
    S9,
    S10,
    S11,
    T3,
    T4,
    T5,
    T6,
    Pc,
}

impl From<u32> for Register {
    fn from(val: u32) -> Self {
        assert!(val < 32);
        unsafe {
            core::ptr::read_unaligned(&(val as usize) as
                                      *const usize as *const Register)
        }
    }
}

impl Emulator {
    /// Creates a new emulator with `size` bytes of memory
    pub fn new(size: usize) -> Self {
        Emulator {
            memory:     Mmu::new(size),
            registers:  [0; 33],
            fuzz_input: Vec::new(),
            files: Files(vec![
                Some(EmuFile::Stdin),
                Some(EmuFile::Stdout),
                Some(EmuFile::Stderr),
            ]),
            jit_cache: None,
            breakpoints: BTreeMap::new(),
        }
    }

    /// Fork an emulator into a new emulator which will diff from the original
    pub fn fork(&self) -> Self {
        Emulator {
            memory:      self.memory.fork(),
            registers:   self.registers.clone(),
            fuzz_input:  self.fuzz_input.clone(),
            files:       self.files.clone(),
            jit_cache:   self.jit_cache.clone(),
            breakpoints: self.breakpoints.clone(),
        }
    }

    /// Enable the JIT and use a specified `JitCache`
    pub fn enable_jit(mut self, jit_cache: Arc<JitCache>) -> Self {
        self.jit_cache = Some(jit_cache);
        self
    }
    
    /// Register a new breakpoint callback
    pub fn add_breakpoint(&mut self, pc: VirtAddr,
                          callback: BreakpointCallback) {
        self.breakpoints.insert(pc, callback);
    }

    /// Reset the state of `self` to `other`, assuming that `self` is
    /// forked off of `other`. If it is not, the results are invalid.
    pub fn reset(&mut self, other: &Self) {
        // Reset memory state
        self.memory.reset(&other.memory);

        // Reset register state
        self.registers = other.registers;

        // Reset file state
        self.files.0.clear();
        self.files.0.extend_from_slice(&other.files.0);
    }

    /// Allocate a new file descriptor
    pub fn alloc_file(&mut self) -> usize {
        for (fd, file) in self.files.0.iter().enumerate() {
            if file.is_none() {
                // File not present, we can reuse the FD
                return fd;
            }
        }
        
        // If we got here, no FD is present, create a new one
        let fd = self.files.0.len();
        self.files.0.push(None);
        fd
    }

    /// Get a register from the guest
    pub fn reg(&self, register: Register) -> u64 {
        if register != Register::Zero {
            self.registers[register as usize]
        } else {
            0
        }
    }
    
    /// Set a register in the guest
    pub fn set_reg(&mut self, register: Register, val: u64) {
        if register != Register::Zero {
            self.registers[register as usize] = val;
        }
    }

    /// Run the VM using either the emulator or the JIT
    pub fn run(&mut self, instrs_execed: &mut u64, corpus: &Corpus)
            -> Result<(), VmExit> {
        if self.jit_cache.is_some() {
            self.run_jit(instrs_execed, corpus)
        } else {
            self.run_emu(instrs_execed, corpus)
        }
    }

    /// Run the VM using the emulator
    pub fn run_emu(&mut self, instrs_execed: &mut u64, corpus: &Corpus)
            -> Result<(), VmExit> {
        'next_inst: loop {
            // Get the current program counter
            let pc = self.reg(Register::Pc);
            
            // Check alignment
            if pc & 3 != 0 {
                // Code was unaligned, return a code fetch fault
                return Err(VmExit::ExecFault(VirtAddr(pc as usize)));
            }

            // Read the instruction
            let inst: u32 = self.memory.read_perms(VirtAddr(pc as usize), 
                                                   Perm(PERM_EXEC))
                .map_err(|x| VmExit::ExecFault(x.is_crash().unwrap().1))?;

            // Update number of instructions executed
            *instrs_execed += 1;

            // Extract the opcode from the instruction
            let opcode = inst & 0b1111111;

            // Update code coverage
            corpus.code_coverage.entry_or_insert(
                &VirtAddr(pc as usize), pc as usize, || {
                    // Save the input and log it in the hash table
                    let hash = corpus.hasher.hash(&self.fuzz_input);
                    corpus.input_hashes.entry_or_insert(
                            &hash, hash as usize, || {
                        corpus.inputs.push(Box::new(self.fuzz_input.clone()));
                        Box::new(())
                    });

                    Box::new(())
                });

            //print!("{}\n\n", self);

            match opcode {
                0b0110111 => {
                    // LUI
                    let inst = Utype::from(inst);
                    self.set_reg(inst.rd, inst.imm as i64 as u64);
                }
                0b0010111 => {
                    // AUIPC
                    let inst = Utype::from(inst);
                    self.set_reg(inst.rd,
                                 (inst.imm as i64 as u64).wrapping_add(pc));
                }
                0b1101111 => {
                    // JAL
                    let inst = Jtype::from(inst);
                    self.set_reg(inst.rd, pc.wrapping_add(4));
                    self.set_reg(Register::Pc,
                                 pc.wrapping_add(inst.imm as i64 as u64));
                    continue 'next_inst;
                }
                0b1100111 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);

                    match inst.funct3 {
                        0b000 => {
                            // JALR
                            let target = self.reg(inst.rs1).wrapping_add(
                                    inst.imm as i64 as u64);
                            self.set_reg(inst.rd, pc.wrapping_add(4));
                            self.set_reg(Register::Pc, target);
                            continue 'next_inst;
                        }
                        _ => unimplemented!("Unexpected 0b1100111"),
                    }
                }
                0b1100011 => {
                    // We know it's an Btype
                    let inst = Btype::from(inst);

                    let rs1 = self.reg(inst.rs1);
                    let rs2 = self.reg(inst.rs2);

                    match inst.funct3 {
                        0b000 => {
                            // BEQ
                            if rs1 == rs2 {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b001 => {
                            // BNE
                            if rs1 != rs2 {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b100 => {
                            // BLT
                            if (rs1 as i64) < (rs2 as i64) {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b101 => {
                            // BGE
                            if (rs1 as i64) >= (rs2 as i64) {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b110 => {
                            // BLTU
                            if (rs1 as u64) < (rs2 as u64) {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b111 => {
                            // BGEU
                            if (rs1 as u64) >= (rs2 as u64) {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        _ => unimplemented!("Unexpected 0b1100011"),
                    }
                }
                0b0000011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);

                    // Compute the address
                    let addr = VirtAddr(self.reg(inst.rs1)
                        .wrapping_add(inst.imm as i64 as u64)
                        as usize);

                    match inst.funct3 {
                        0b000 => {
                            // LB
                            let mut tmp = [0u8; 1];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                i8::from_le_bytes(tmp) as i64 as u64);
                        }
                        0b001 => {
                            // LH
                            let mut tmp = [0u8; 2];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                i16::from_le_bytes(tmp) as i64 as u64);
                        }
                        0b010 => {
                            // LW
                            let mut tmp = [0u8; 4];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                i32::from_le_bytes(tmp) as i64 as u64);
                        }
                        0b011 => {
                            // LD
                            let mut tmp = [0u8; 8];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                i64::from_le_bytes(tmp) as i64 as u64);
                        }
                        0b100 => {
                            // LBU
                            let mut tmp = [0u8; 1];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                u8::from_le_bytes(tmp) as u64);
                        }
                        0b101 => {
                            // LHU
                            let mut tmp = [0u8; 2];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                u16::from_le_bytes(tmp) as u64);
                        }
                        0b110 => {
                            // LWU
                            let mut tmp = [0u8; 4];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                u32::from_le_bytes(tmp) as u64);
                        }
                        _ => unimplemented!("Unexpected 0b0000011"),
                    }
                }
                0b0100011 => {
                    // We know it's an Stype
                    let inst = Stype::from(inst);

                    // Compute the address
                    let addr = VirtAddr(self.reg(inst.rs1)
                        .wrapping_add(inst.imm as i64 as u64)
                        as usize);

                    match inst.funct3 {
                        0b000 => {
                            // SB
                            let val = self.reg(inst.rs2) as u8;
                            self.memory.write(addr, val)?;
                        }
                        0b001 => {
                            // SH
                            let val = self.reg(inst.rs2) as u16;
                            self.memory.write(addr, val)?;
                        }
                        0b010 => {
                            // SW
                            let val = self.reg(inst.rs2) as u32;
                            self.memory.write(addr, val)?;
                        }
                        0b011 => {
                            // SD
                            let val = self.reg(inst.rs2) as u64;
                            self.memory.write(addr, val)?;
                        }
                        _ => unimplemented!("Unexpected 0b0100011"),
                    }
                }
                0b0010011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);
                    
                    let rs1 = self.reg(inst.rs1);
                    let imm = inst.imm as i64 as u64;

                    match inst.funct3 {
                        0b000 => {
                            // ADDI
                            self.set_reg(inst.rd, rs1.wrapping_add(imm));
                        }
                        0b010 => {
                            // SLTI
                            if (rs1 as i64) < (imm as i64) {
                                self.set_reg(inst.rd, 1);
                            } else {
                                self.set_reg(inst.rd, 0);
                            }
                        }
                        0b011 => {
                            // SLTIU
                            if (rs1 as u64) < (imm as u64) {
                                self.set_reg(inst.rd, 1);
                            } else {
                                self.set_reg(inst.rd, 0);
                            }
                        }
                        0b100 => {
                            // XORI
                            self.set_reg(inst.rd, rs1 ^ imm);
                        }
                        0b110 => {
                            // ORI
                            self.set_reg(inst.rd, rs1 | imm);
                        }
                        0b111 => {
                            // ANDI
                            self.set_reg(inst.rd, rs1 & imm);
                        }
                        0b001 => {
                            let mode = (inst.imm >> 6) & 0b111111;
                            
                            match mode {
                                0b000000 => {
                                    // SLLI
                                    let shamt = inst.imm & 0b111111;
                                    self.set_reg(inst.rd, rs1 << shamt);
                                }
                                _ => unreachable!(),
                            }
                        }
                        0b101 => {
                            let mode = (inst.imm >> 6) & 0b111111;
                            
                            match mode {
                                0b000000 => {
                                    // SRLI
                                    let shamt = inst.imm & 0b111111;
                                    self.set_reg(inst.rd, rs1 >> shamt);
                                }
                                0b010000 => {
                                    // SRAI
                                    let shamt = inst.imm & 0b111111;
                                    self.set_reg(inst.rd,
                                        ((rs1 as i64) >> shamt) as u64);
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                0b0110011 => {
                    // We know it's an Rtype
                    let inst = Rtype::from(inst);

                    let rs1 = self.reg(inst.rs1);
                    let rs2 = self.reg(inst.rs2);

                    match (inst.funct7, inst.funct3) {
                        (0b0000000, 0b000) => {
                            // ADD
                            self.set_reg(inst.rd, rs1.wrapping_add(rs2));
                        }
                        (0b0100000, 0b000) => {
                            // SUB
                            self.set_reg(inst.rd, rs1.wrapping_sub(rs2));
                        }
                        (0b0000000, 0b001) => {
                            // SLL
                            let shamt = rs2 & 0b111111;
                            self.set_reg(inst.rd, rs1 << shamt);
                        }
                        (0b0000000, 0b010) => {
                            // SLT
                            if (rs1 as i64) < (rs2 as i64) {
                                self.set_reg(inst.rd, 1);
                            } else {
                                self.set_reg(inst.rd, 0);
                            }
                        }
                        (0b0000000, 0b011) => {
                            // SLTU
                            if (rs1 as u64) < (rs2 as u64) {
                                self.set_reg(inst.rd, 1);
                            } else {
                                self.set_reg(inst.rd, 0);
                            }
                        }
                        (0b0000000, 0b100) => {
                            // XOR
                            self.set_reg(inst.rd, rs1 ^ rs2);
                        }
                        (0b0000000, 0b101) => {
                            // SRL
                            let shamt = rs2 & 0b111111;
                            self.set_reg(inst.rd, rs1 >> shamt);
                        }
                        (0b0100000, 0b101) => {
                            // SRA
                            let shamt = rs2 & 0b111111;
                            self.set_reg(inst.rd,
                                ((rs1 as i64) >> shamt) as u64);
                        }
                        (0b0000000, 0b110) => {
                            // OR
                            self.set_reg(inst.rd, rs1 | rs2);
                        }
                        (0b0000000, 0b111) => {
                            // AND
                            self.set_reg(inst.rd, rs1 & rs2);
                        }
                        _ => unreachable!(),
                    }
                }
                0b0111011 => {
                    // We know it's an Rtype
                    let inst = Rtype::from(inst);

                    let rs1 = self.reg(inst.rs1) as u32;
                    let rs2 = self.reg(inst.rs2) as u32;

                    match (inst.funct7, inst.funct3) {
                        (0b0000000, 0b000) => {
                            // ADDW
                            self.set_reg(inst.rd,
                                rs1.wrapping_add(rs2) as i32 as i64 as u64);
                        }
                        (0b0100000, 0b000) => {
                            // SUBW
                            self.set_reg(inst.rd,
                                rs1.wrapping_sub(rs2) as i32 as i64 as u64);
                        }
                        (0b0000000, 0b001) => {
                            // SLLW
                            let shamt = rs2 & 0b11111;
                            self.set_reg(inst.rd,
                                (rs1 << shamt) as i32 as i64 as u64);
                        }
                        (0b0000000, 0b101) => {
                            // SRLW
                            let shamt = rs2 & 0b11111;
                            self.set_reg(inst.rd,
                                (rs1 >> shamt) as i32 as i64 as u64);
                        }
                        (0b0100000, 0b101) => {
                            // SRAW
                            let shamt = rs2 & 0b11111;
                            self.set_reg(inst.rd,
                                ((rs1 as i32) >> shamt) as i64 as u64);
                        }
                        _ => unreachable!(),
                    }
                }
                0b0001111 => {
                    let inst = Itype::from(inst);

                    match inst.funct3 {
                        0b000 => {
                            // FENCE
                        }
                        _ => unreachable!(),
                    }
                }
                0b1110011 => {
                    if inst == 0b00000000000000000000000001110011 {
                        // ECALL
                        return Err(VmExit::Syscall);
                    } else if inst == 0b00000000000100000000000001110011 {
                        // EBREAK
                        panic!("EBREAK");
                    } else {
                        unreachable!();
                    }
                }
                0b0011011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);
                    
                    let rs1 = self.reg(inst.rs1) as u32;
                    let imm = inst.imm as u32;

                    match inst.funct3 {
                        0b000 => {
                            // ADDIW
                            self.set_reg(inst.rd,
                                rs1.wrapping_add(imm) as i32 as i64 as u64);
                        }
                        0b001 => {
                            let mode = (inst.imm >> 5) & 0b1111111;
                            
                            match mode {
                                0b0000000 => {
                                    // SLLIW
                                    let shamt = inst.imm & 0b11111;
                                    self.set_reg(inst.rd,
                                        (rs1 << shamt) as i32 as i64 as u64);
                                }
                                _ => unreachable!(),
                            }
                        }
                        0b101 => {
                            let mode = (inst.imm >> 5) & 0b1111111;
                            
                            match mode {
                                0b0000000 => {
                                    // SRLIW
                                    let shamt = inst.imm & 0b11111;
                                    self.set_reg(inst.rd,
                                        (rs1 >> shamt) as i32 as i64 as u64)
                                }
                                0b0100000 => {
                                    // SRAIW
                                    let shamt = inst.imm & 0b11111;
                                    self.set_reg(inst.rd,
                                        ((rs1 as i32) >> shamt) as i64 as u64);
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                _ => unimplemented!("Unhandled opcode {:#09b}\n", opcode),
            }

            // Update PC to the next instruction
            self.set_reg(Register::Pc, pc.wrapping_add(4));
        }
    }
    
    /// Run the VM using the JIT
    pub fn run_jit(&mut self, instrs_execed: &mut u64, corpus: &Corpus)
            -> Result<(), VmExit> {
        // Get the JIT addresses
        let (memory, perms, dirty, dirty_bitmap) = self.memory.jit_addrs();

        // Get the translation table
        let trans_table = self.jit_cache.as_ref().unwrap().translation_table();

        // If `Some`, we re-entry the JIT by jumping directly to this address,
        // ignoring PC
        let mut override_jit_addr = None;

        loop {
            let jit_addr = if let Some(override_jit_addr) =
                    override_jit_addr.take() {
                override_jit_addr
            } else {
                // Get the current PC
                let pc = self.reg(Register::Pc);
                let (jit_addr, num_blocks) = {
                    let jit_cache = self.jit_cache.as_ref().unwrap();
                    (
                        jit_cache.lookup(VirtAddr(pc as usize)),
                        jit_cache.num_blocks()
                    )
                };

                if let Some(jit_addr) = jit_addr {
                    jit_addr
                } else {
                    // Go through each instruction in the block, and accumulate
                    // an assembly string which we will assemble using `nasm`
                    // om the command line
                    let asm = self.generate_jit(
                        VirtAddr(pc as usize), num_blocks, corpus)?;

                    // Write out the assembly
                    let asmfn = std::env::temp_dir().join(
                        format!("fwetmp_{:?}.asm",
                                std::thread::current().id()));
                    let binfn = std::env::temp_dir().join(
                        format!("fwetmp_{:?}.bin",
                                std::thread::current().id()));
                    std::fs::write(&asmfn, &asm)
                        .expect("Failed to write out asm");

                    // Invoke NASM to generate the binary
                    let res = Command::new("nasm").args(&[
                        "-f", "bin", "-o", binfn.to_str().unwrap(),
                        asmfn.to_str().unwrap()
                    ]).status()
                        .expect("Failed to run nasm, is it in your path?");
                    assert!(res.success(), "nasm returned an error");

                    // Read the binary
                    let tmp = std::fs::read(&binfn)
                        .expect("Failed to read nasm output");

                    // Update the JIT tables
                    self.jit_cache.as_ref().unwrap().add_mapping(
                        VirtAddr(pc as usize), &tmp)
                }
            };

            unsafe {
                // Invoke the jit
                let exit_code:  u64;
                let reentry_pc: u64;
                let exit_info: u64;
            
                let dirty_inuse = self.memory.dirty_len();
                let new_dirty_inuse: usize;
                let mut instcount = *instrs_execed;

                asm!(r#"
                    call {entry}
                "#,
                entry = in(reg) jit_addr,
                out("rax") exit_code,
                out("rbx") reentry_pc,
                out("rcx") exit_info,
                out("rdx") _,
                in("r8")  memory,
                in("r9")  perms,
                in("r10") dirty,
                in("r11") dirty_bitmap,
                inout("r12") dirty_inuse => new_dirty_inuse,
                in("r13") self.registers.as_ptr(),
                in("r14") trans_table,
                inout("r15") instcount,
                );
                
                // Update the PC reentry point
                self.set_reg(Register::Pc, reentry_pc);

                // Update instrs execed
                *instrs_execed = instcount;

                // Update the dirty state
                self.memory.set_dirty_len(new_dirty_inuse);

                match exit_code {
                    1 => {
                        // Branch decode request, just continue as PC has been
                        // updated to the new target
                    }
                    2 => {
                        // Syscall
                        return Err(VmExit::Syscall);
                    }
                    4 => {
                        // Read fault
                        // The JIT reports the address of the base of the
                        // access, invoke the emulator to get the specific
                        // byte which caused the fault
                        return self.run_emu(instrs_execed, corpus);
                    }
                    5 => {
                        // Write fault
                        // The JIT reports the address of the base of the
                        // access, invoke the emulator to get the specific
                        // byte which caused the fault
                        return self.run_emu(instrs_execed, corpus);
                    }
                    6 => {
                        // Hit the instruction count timeout
                        return Err(VmExit::Timeout);
                    }
                    7 => {
                        // Hit breakpoint, invoke callback
                        let pc = VirtAddr(reentry_pc as usize);
                        if let Some(callback) = self.breakpoints.get(&pc) {
                            callback(self)?;
                        }

                        if self.reg(Register::Pc) == reentry_pc {
                            // Force execution at the return location, which
                            // will skip over the breakpoint return
                            override_jit_addr = Some(exit_info as usize);
                        } else {
                            // PC was changed by the breakpoint handler,
                            // thus we respect its change and will jump
                            // to the target it specified
                        }
                    }
                    8 => {
                        // An invalid opcode was executed
                        return Err(VmExit::InvalidOpcode);
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    /// Generates the assembly string for `pc` during JIT
    pub fn generate_jit(&self, pc: VirtAddr, num_blocks: usize,
                        corpus: &Corpus) -> Result<String, VmExit> {
        let mut asm = "[bits 64]\n".to_string();

        // First in the block, check for an instruction timeout
        asm += &format!(r#"
            cmp r15, 1_000_000_000
            jb  no_timeout

            mov rax, 6
            mov rbx, {pc}
            ret

            no_timeout:
        "#, pc = pc.0);

        let mut pc = pc.0 as u64;
        let mut block_instrs = 0;
        'next_inst: loop {
            // Check alignment
            if pc & 3 != 0 {
                // Code was unaligned, return a code fetch fault
                return Err(VmExit::ExecFault(VirtAddr(pc as usize)));
            }

            // Read the instruction
            let inst: u32 = self.memory.read_perms(VirtAddr(pc as usize), 
                                                   Perm(PERM_EXEC))
                .map_err(|x| VmExit::ExecFault(x.is_crash().unwrap().1))?;

            // Extract the opcode from the instruction
            let opcode = inst & 0b1111111;

            // Add a label to this instruction
            asm += &format!("inst_pc_{:#x}:\n", pc);

            // Insert breakpoint if needed
            if self.breakpoints.contains_key(&VirtAddr(pc as usize)) {
                asm += &format!(r#"
                    lea rcx, [rel .after_bp]
                    mov rax, 7
                    mov rbx, {pc}
                    ret

                    .after_bp:
                "#, pc = pc);
            }
            
            // Update code coverage
            corpus.code_coverage.entry_or_insert(
                &VirtAddr(pc as usize), pc as usize, || {
                    // Save the input and log it in the hash table
                    let hash = corpus.hasher.hash(&self.fuzz_input);
                    corpus.input_hashes.entry_or_insert(
                            &hash, hash as usize, || {
                        corpus.inputs.push(Box::new(self.fuzz_input.clone()));
                        Box::new(())
                    });

                    Box::new(())
                });

            // Track number of instructions in the block
            block_instrs += 1;

            // Produce the assembly statement to load RISC-V `reg` into
            // `x86reg`
            macro_rules! load_reg {
                ($x86reg:expr, $reg:expr) => {
                    if $reg == Register::Zero {
                        format!("xor {x86reg}, {x86reg}\n", x86reg = $x86reg)
                    } else {
                        format!("mov {x86reg}, qword [r13 + {reg}*8]\n",
                            x86reg = $x86reg, reg = $reg as usize)
                    }
                }
            }
            
            // Produce the assembly statement to store RISC-V `reg` from
            // `x86reg`
            macro_rules! store_reg {
                ($reg:expr, $x86reg:expr) => {
                    if $reg == Register::Zero {
                        String::new()
                    } else {
                        format!("mov qword [r13 + {reg}*8], {x86reg}\n",
                            x86reg = $x86reg, reg = $reg as usize)
                    }
                }
            }

            match opcode {
                0b0110111 => {
                    // LUI
                    let inst = Utype::from(inst);
                    asm += &store_reg!(inst.rd, inst.imm);
                }
                0b0010111 => {
                    // AUIPC
                    let inst = Utype::from(inst);
                    let val = (inst.imm as i64 as u64).wrapping_add(pc);
                    asm += &format!(r#"
                        mov rax, {imm:#x}
                        {store_rd_from_rax}
                    "#, imm = val,
                        store_rd_from_rax = store_reg!(inst.rd, "rax"));
                }
                0b1101111 => {
                    // JAL
                    let inst = Jtype::from(inst);

                    // Compute the return address
                    let ret = pc.wrapping_add(4);

                    // Compute the branch target
                    let target = pc.wrapping_add(inst.imm as i64 as u64);

                    if (target / 4) >= num_blocks as u64 {
                        // Branch target is out of bounds
                        panic!("JITOOB");
                    }
                    
                    asm += &format!(r#"
                        mov rax, {ret}
                        {store_rd_from_rax}
                       
                        mov  rax, [r14 + {target}]
                        test rax, rax
                        jz   .jit_resolve

                        add r15, {block_instrs}
                        jmp rax

                        .jit_resolve:
                        mov rax, 1
                        mov rbx, {target_pc}
                        add r15, {block_instrs}
                        ret

                    "#, ret = ret,
                        target_pc = target,
                        store_rd_from_rax = store_reg!(inst.rd, "rax"),
                        block_instrs = block_instrs,
                        target = (target / 4) * 8);
                    break 'next_inst;
                }
                0b1100111 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);

                    match inst.funct3 {
                        0b000 => {
                            // JALR
                            
                            // Compute the return address
                            let ret = pc.wrapping_add(4);
                            
                            asm += &format!(r#"
                                mov rax, {ret}
                                {store_rd_from_rax}

                                {load_rax_from_rs1}
                                add rax, {imm}

                                shr rax, 2
                                cmp rax, {num_blocks}
                                jae .jit_resolve
                               
                                mov  rax, [r14 + rax*8]
                                test rax, rax
                                jz   .jit_resolve

                                add r15, {block_instrs}
                                jmp rax

                                .jit_resolve:
                                {load_rbx_from_rs1}
                                add rbx, {imm}
                                mov rax, 1
                                add r15, {block_instrs}
                                ret

                            "#, imm = inst.imm,
                                ret = ret,
                                store_rd_from_rax = store_reg!(inst.rd, "rax"),
                                load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs1 = load_reg!("rbx", inst.rs1),
                                block_instrs = block_instrs,
                                num_blocks = num_blocks);
                            break 'next_inst;
                        }
                        _ => unimplemented!("Unexpected 0b1100111"),
                    }
                }
                0b1100011 => {
                    // We know it's an Btype
                    let inst = Btype::from(inst);

                    match inst.funct3 {
                        0b000 | 0b001 | 0b100 | 0b101 | 0b110 | 0b111 => {
                            let cond = match inst.funct3 {
                                0b000 => /* BEQ  */ "jne",
                                0b001 => /* BNE  */ "je",
                                0b100 => /* BLT  */ "jnl",
                                0b101 => /* BGE  */ "jnge",
                                0b110 => /* BLTU */ "jnb",
                                0b111 => /* BGEU */ "jnae",
                                _ => unreachable!(),
                            };

                            let target =
                                pc.wrapping_add(inst.imm as i64 as u64);
                    
                            if (target / 4) >= num_blocks as u64 {
                                // Branch target is out of bounds
                                panic!("JITOOB");
                            }

                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}

                                cmp rax, rbx
                                {cond} .fallthrough

                                mov  rax, [r14 + {target}]
                                test rax, rax
                                jz   .jit_resolve

                                add r15, {block_instrs}
                                jmp rax
                        
                                .jit_resolve:
                                mov rax, 1
                                mov rbx, {target_pc}
                                add r15, {block_instrs}
                                ret
                            
                                .fallthrough:
                            "#, cond = cond,
                                load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                target_pc = target,
                                block_instrs = block_instrs,
                                target = (target / 4) * 8);
                        }
                        _ => unimplemented!("Unexpected 0b1100011"),
                    }
                }
                0b0000011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);

                    let (loadtyp, loadsz, regtyp, access_size) =
                            match inst.funct3 {
                        0b000 => /* LB  */ ("movsx", "byte",  "rbx", 1),
                        0b001 => /* LH  */ ("movsx", "word",  "rbx", 2),
                        0b010 => /* LW  */ ("movsx", "dword", "rbx", 4),
                        0b011 => /* LD  */ ("mov",   "qword", "rbx", 8),
                        0b100 => /* LBU */ ("movzx", "byte",  "rbx", 1),
                        0b101 => /* LHU */ ("movzx", "word",  "rbx", 2),
                        0b110 => /* LWU */ ("mov",   "dword", "ebx", 4),
                        _ => unreachable!(),
                    };

                    // Compute the read permission mask
                    let mut perm_mask = 0u64;
                    for ii in 0..access_size {
                        perm_mask |= (PERM_READ as u64) << (ii * 8)
                    }

                    asm += &format!(r#"
                        {load_rax_from_rs1}
                        add rax, {imm}

                        cmp rax, {memory_len} - {access_size}
                        ja  .fault

                        {loadtyp} {regtyp}, {loadsz} [r9 + rax]
                        mov rcx, {perm_mask}
                        and rbx, rcx
                        cmp rbx, rcx
                        je  .nofault

                        .fault:
                        mov rcx, rax
                        mov rbx, {pc}
                        mov rax, 4
                        add r15, {block_instrs}
                        ret

                        .nofault:
                        {loadtyp} {regtyp}, {loadsz} [r8 + rax]
                        {store_rbx_into_rd}
                    "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                        store_rbx_into_rd = store_reg!(inst.rd, "rbx"),
                        loadtyp = loadtyp,
                        loadsz = loadsz,
                        regtyp = regtyp,
                        pc = pc,
                        access_size = access_size,
                        block_instrs = block_instrs,
                        perm_mask = perm_mask,
                        memory_len = self.memory.len(),
                        imm = inst.imm);
                }
                0b0100011 => {
                    // We know it's an Stype
                    let inst = Stype::from(inst);

                    let (loadtyp, loadsz, regtype, loadrt, access_size) =
                            match inst.funct3 {
                        0b000 => /* SB */ ("movzx", "byte",  "bl",  "ebx", 1),
                        0b001 => /* SH */ ("movzx", "word",  "bx",  "ebx", 2),
                        0b010 => /* SW */ ("mov",   "dword", "ebx", "ebx", 4),
                        0b011 => /* SD */ ("mov",   "qword", "rbx", "rbx", 8),
                        _ => unreachable!(),
                    };

                    // Make sure the dirty block size is sane
                    assert!(DIRTY_BLOCK_SIZE.count_ones() == 1 &&
                            DIRTY_BLOCK_SIZE >= 8,
                        "Dirty block size must be a power of two and >= 8");

                    // Amount to shift to get the block from an address
                    let dirty_block_shift = DIRTY_BLOCK_SIZE.trailing_zeros();
                    
                    // Compute the write permission mask
                    let mut perm_mask = 0u64;
                    for ii in 0..access_size {
                        perm_mask |= (PERM_WRITE as u64) << (ii * 8)
                    }

                    asm += &format!(r#"
                        {load_rax_from_rs1}
                        add rax, {imm}

                        cmp rax, {memory_len} - {access_size}
                        ja  .fault

                        {loadtyp} {loadrt}, {loadsz} [r9 + rax]
                        mov rcx, {perm_mask}
                        mov rdx, rbx
                        and rbx, rcx
                        cmp rbx, rcx
                        je  .nofault

                        .fault:
                        mov rcx, rax
                        mov rbx, {pc}
                        mov rax, 5
                        add r15, {block_instrs}
                        ret

                        .nofault:
                        ; Get the raw bits and shift them into the read slot
                        shl rcx, 2
                        and rdx, rcx
                        shr rdx, 3
                        mov rbx, rdx
                        or {loadsz} [r9 + rax], {regtype}

                        mov rcx, rax
                        shr rcx, {dirty_block_shift}
                        bts qword [r11], rcx
                        jc  .continue

                        mov qword [r10 + r12*8], rcx
                        add r12, 1

                        .continue:
                        {load_rbx_from_rs2}
                        mov {loadsz} [r8 + rax], {regtype}
                    "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                        load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                        loadtyp = loadtyp,
                        loadsz = loadsz,
                        regtype = regtype,
                        imm = inst.imm,
                        loadrt = loadrt,
                        access_size = access_size,
                        block_instrs = block_instrs,
                        perm_mask = perm_mask,
                        memory_len = self.memory.len(),
                        pc = pc,
                        dirty_block_shift = dirty_block_shift);
                }
                0b0010011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);

                    match inst.funct3 {
                        0b000 => {
                            // ADDI
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                add rax, {imm}
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                store_rax_into_rd = store_reg!(inst.rd, "rax"),
                                imm = inst.imm);
                        }
                        0b010 => {
                            // SLTI
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                xor  ebx, ebx
                                cmp  rax, {imm}
                                setl bl
                                {store_rbx_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                store_rbx_into_rd = store_reg!(inst.rd, "rbx"),
                                imm = inst.imm);
                        }
                        0b011 => {
                            // SLTIU
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                xor  ebx, ebx
                                cmp  rax, {imm}
                                setb bl
                                {store_rbx_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                store_rbx_into_rd = store_reg!(inst.rd, "rbx"),
                                imm = inst.imm);
                        }
                        0b100 => {
                            // XORI
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                xor rax, {imm}
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                store_rax_into_rd = store_reg!(inst.rd, "rax"),
                                imm = inst.imm);
                        }
                        0b110 => {
                            // ORI
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                or rax, {imm}
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                store_rax_into_rd = store_reg!(inst.rd, "rax"),
                                imm = inst.imm);
                        }
                        0b111 => {
                            // ANDI
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                and rax, {imm}
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                store_rax_into_rd = store_reg!(inst.rd, "rax"),
                                imm = inst.imm);
                        }
                        0b001 => {
                            let mode = (inst.imm >> 6) & 0b111111;
                            
                            match mode {
                                0b000000 => {
                                    // SLLI
                                    let shamt = inst.imm & 0b111111;
                                    asm += &format!(r#"
                                        {load_rax_from_rs1}
                                        shl rax, {imm}
                                        {store_rax_into_rd}
                                    "#, load_rax_from_rs1 =
                                            load_reg!("rax", inst.rs1),
                                        store_rax_into_rd =
                                            store_reg!(inst.rd, "rax"),
                                        imm = shamt);
                                }
                                _ => unreachable!(),
                            }
                        }
                        0b101 => {
                            let mode = (inst.imm >> 6) & 0b111111;
                            
                            match mode {
                                0b000000 => {
                                    // SRLI
                                    let shamt = inst.imm & 0b111111;
                                    asm += &format!(r#"
                                        {load_rax_from_rs1}
                                        shr rax, {imm}
                                        {store_rax_into_rd}
                                    "#, load_rax_from_rs1 =
                                            load_reg!("rax", inst.rs1),
                                        store_rax_into_rd =
                                            store_reg!(inst.rd, "rax"),
                                        imm = shamt);
                                }
                                0b010000 => {
                                    // SRAI
                                    let shamt = inst.imm & 0b111111;
                                    asm += &format!(r#"
                                        {load_rax_from_rs1}
                                        sar rax, {imm}
                                        {store_rax_into_rd}
                                    "#, load_rax_from_rs1 =
                                            load_reg!("rax", inst.rs1),
                                        store_rax_into_rd =
                                            store_reg!(inst.rd, "rax"),
                                        imm = shamt);
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                0b0110011 => {
                    // We know it's an Rtype
                    let inst = Rtype::from(inst);

                    match (inst.funct7, inst.funct3) {
                        (0b0000000, 0b000) => {
                            // ADD
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}
                                add rax, rbx
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0100000, 0b000) => {
                            // SUB
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}
                                sub rax, rbx
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0000000, 0b001) => {
                            // SLL
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rcx_from_rs2}
                                shl rax, cl
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rcx_from_rs2 = load_reg!("rcx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0000000, 0b010) => {
                            // SLT
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}
                                xor  ecx, ecx
                                cmp  rax, rbx
                                setl cl
                                {store_rcx_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                store_rcx_into_rd = store_reg!(inst.rd, "rcx")
                                );
                        }
                        (0b0000000, 0b011) => {
                            // SLTU
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}
                                xor  ecx, ecx
                                cmp  rax, rbx
                                setb cl
                                {store_rcx_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                store_rcx_into_rd = store_reg!(inst.rd, "rcx")
                                );
                        }
                        (0b0000000, 0b100) => {
                            // XOR
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}
                                xor rax, rbx
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0000000, 0b101) => {
                            // SRL
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rcx_from_rs2}
                                shr rax, cl
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rcx_from_rs2 = load_reg!("rcx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0100000, 0b101) => {
                            // SRA
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rcx_from_rs2}
                                sar rax, cl
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rcx_from_rs2 = load_reg!("rcx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0000000, 0b110) => {
                            // OR
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}
                                or rax, rbx
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0000000, 0b111) => {
                            // AND
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}
                                and rax, rbx
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        _ => unreachable!(),
                    }
                }
                0b0111011 => {
                    // We know it's an Rtype
                    let inst = Rtype::from(inst);

                    match (inst.funct7, inst.funct3) {
                        (0b0000000, 0b000) => {
                            // ADDW
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}
                                add eax, ebx
                                movsx rax, eax
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0100000, 0b000) => {
                            // SUBW
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rbx_from_rs2}
                                sub eax, ebx
                                movsx rax, eax
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rbx_from_rs2 = load_reg!("rbx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0000000, 0b001) => {
                            // SLLW
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rcx_from_rs2}
                                shl eax, cl
                                movsx rax, eax
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rcx_from_rs2 = load_reg!("rcx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0000000, 0b101) => {
                            // SRLW
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rcx_from_rs2}
                                shr eax, cl
                                movsx rax, eax
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rcx_from_rs2 = load_reg!("rcx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        (0b0100000, 0b101) => {
                            // SRAW
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                {load_rcx_from_rs2}
                                sar eax, cl
                                movsx rax, eax
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                load_rcx_from_rs2 = load_reg!("rcx", inst.rs2),
                                store_rax_into_rd = store_reg!(inst.rd, "rax")
                                );
                        }
                        _ => unreachable!(),
                    }
                }
                0b0001111 => {
                    let inst = Itype::from(inst);

                    match inst.funct3 {
                        0b000 => {
                            // FENCE
                        }
                        _ => unreachable!(),
                    }
                }
                0b1110011 => {
                    if inst == 0b00000000000000000000000001110011 {
                        // ECALL
                        asm += &format!(r#"
                            mov rax, 2
                            mov rbx, {pc}
                            add r15, {block_instrs}
                            ret
                        "#, pc = pc, block_instrs = block_instrs);
                    } else if inst == 0b00000000000100000000000001110011 {
                        // EBREAK
                        asm += &format!(r#"
                            mov rax, 3
                            mov rbx, {pc}
                            add r15, {block_instrs}
                            ret
                        "#, pc = pc, block_instrs = block_instrs);
                    } else {
                        unreachable!();
                    }
                }
                0b0011011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);
                    
                    match inst.funct3 {
                        0b000 => {
                            // ADDIW
                            asm += &format!(r#"
                                {load_rax_from_rs1}
                                add eax, {imm}
                                movsx rax, eax
                                {store_rax_into_rd}
                            "#, load_rax_from_rs1 = load_reg!("rax", inst.rs1),
                                store_rax_into_rd = store_reg!(inst.rd, "rax"),
                                imm = inst.imm);
                        }
                        0b001 => {
                            let mode = (inst.imm >> 5) & 0b1111111;
                            
                            match mode {
                                0b0000000 => {
                                    // SLLIW
                                    let shamt = inst.imm & 0b11111;
                                    asm += &format!(r#"
                                        {load_rax_from_rs1}
                                        shl eax, {imm}
                                        movsx rax, eax
                                        {store_rax_into_rd}
                                    "#, load_rax_from_rs1 =
                                            load_reg!("rax", inst.rs1),
                                        store_rax_into_rd =
                                            store_reg!(inst.rd, "rax"),
                                        imm = shamt);
                                }
                                _ => unreachable!(),
                            }
                        }
                        0b101 => {
                            let mode = (inst.imm >> 5) & 0b1111111;
                            
                            match mode {
                                0b0000000 => {
                                    // SRLIW
                                    let shamt = inst.imm & 0b11111;
                                    asm += &format!(r#"
                                        {load_rax_from_rs1}
                                        shr eax, {imm}
                                        movsx rax, eax
                                        {store_rax_into_rd}
                                    "#, load_rax_from_rs1 =
                                            load_reg!("rax", inst.rs1),
                                        store_rax_into_rd =
                                            store_reg!(inst.rd, "rax"),
                                        imm = shamt);
                                }
                                0b0100000 => {
                                    // SRAIW
                                    let shamt = inst.imm & 0b11111;
                                    asm += &format!(r#"
                                        {load_rax_from_rs1}
                                        sar eax, {imm}
                                        movsx rax, eax
                                        {store_rax_into_rd}
                                    "#, load_rax_from_rs1 =
                                            load_reg!("rax", inst.rs1),
                                        store_rax_into_rd =
                                            store_reg!(inst.rd, "rax"),
                                        imm = shamt);
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                _ => {
                    asm += &format!(r#"
                        mov rax, 8
                        mov rbx, {pc}
                        add r15, {block_instrs}
                        ret
                    "#, pc = pc, block_instrs = block_instrs);
                }
            }

            pc += 4;
        }

        Ok(asm)
    }
}

