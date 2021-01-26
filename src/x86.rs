use std::collections::HashMap;
// use std::convert::TryInto;
use crate::ast::{Symbol, intern_string};
use crate::ir::*;

struct X86Assembler {
    machine_code: Vec<u8>,
    functions: HashMap<IrIdent, X86Function>,
    jump_targets: HashMap<IrIdent, X86JumpTarget>,
    debug_break_symbol: Symbol,
    addr_size: isize,
}

struct X86Function {
    prologue_byte_position: usize,
    epilogue_byte_offset: usize,
    byte_length: usize,

    enter_label: IrIdent,
    exit_label: IrIdent,
    return_type: IrType,
    func_address: Option<usize>,
    is_foreign: bool,
}

struct X86JumpTarget {
    label: IrIdent,
    jumps: Vec<usize>,
    pos: usize,
}

#[derive(Debug, Clone, Copy)]
enum X86Opcode {
    NOP,
    MOV,
    MOVSX,
    LEA,
    ADD,
    SUB,
    IMUL,
    IDIV,
    AND,
    OR,
    XOR,
    CDQ,
    CMP,
    TEST,
    SETL,
    SETG,
    SETLE,
    SETGE,
    SETE,
    SETNE,
    JL,
    JLE,
    JG,
    JGE,
    JE,
    JNE,
    JMP,
    PUSH,
    POP,
    CALL,
    RET,
    INT3,
}

#[derive(Debug, Clone, Copy)]
enum X86Operand {
    Stack(isize),
    Register(X86Reg),
    Value(X86Value),
}

#[derive(Debug, Clone, Copy)]
enum X86Reg {
    RAX,
    RCX,
    RDX,
    RBX,
    RSP,
    RBP,
    RSI,
    RDI,
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
}

#[derive(Debug, Clone, Copy)]
enum X86Value {
    Int8(i8),
    Int32(i32),
    Int64(i64),
}

pub fn compile_ir_to_x86_machine_code(
    instructions: Vec<IrInstruction>,
    functions: HashMap<IrIdent, IrBasicBlock>
) -> Vec<u8> {
    let mut x86 = X86Assembler {
        machine_code: Vec::new(),
        functions: HashMap::new(),
        jump_targets: HashMap::new(),
        debug_break_symbol: intern_string("debug_break"),
        addr_size: std::mem::size_of::<usize>() as isize,
    };

    // Register all functions
    for (ident, bb) in &functions {
        let func = X86Function {
            prologue_byte_position: 0,
            epilogue_byte_offset: 0,
            byte_length: 0,

            enter_label:  bb.enter_label,
            exit_label:   bb.exit_label,
            return_type:  bb.return_type,
            func_address: bb.func_address,
            is_foreign:   bb.is_foreign,
        };
        x86.functions.insert(*ident, func);
    }

    // Compile first main function
    let main_ident = create_ir_ident(intern_string("main"), 0);
    if let Some(bb) = functions.get(&main_ident) {
        let insns = &instructions[bb.prologue_index + 1..=bb.epilogue_index];
        push_function(&mut x86, insns, bb);
    } else {
        panic!("x86: failed to locate `main` function");
    }

    // Compile the rest of the functions
    for (ident, bb) in &functions {
        if *ident != main_ident && !bb.is_foreign {
            let insns = &instructions[bb.prologue_index + 1..bb.epilogue_index];
            push_function(&mut x86, insns, bb);
        }
    }

    // Connect jump distances

    return x86.machine_code;
}

fn push_function(x86: &mut X86Assembler, insns: &[IrInstruction], bb: &IrBasicBlock) {

    fn to_x86_operand(variables: &HashMap<IrIdent, X86Operand>, op: IrOperand) -> X86Operand {
        match op {
            IrOperand::Ident(ident) => match variables.get(&ident) {
                Some(operand) => *operand,
                None => {
                    // TODO(alexander): allocate register
                    X86Operand::Register(X86Reg::RAX)
                }
            }
            
            IrOperand::Value(value) => match value {
                IrValue::I32(v)  => X86Operand::Value(X86Value::Int32(v)),
                IrValue::U32(v)  => X86Operand::Value(X86Value::Int32(v as i32)),
                IrValue::U64(v)  => X86Operand::Value(X86Value::Int64(v as i64)),
                IrValue::Bool(v) => X86Operand::Value(X86Value::Int8(v as i8)),
            }
            
            IrOperand::None => panic!("x86: unexpected empty operand"),
        }
    }

    fn insert_variable(variables: &mut HashMap<IrIdent, X86Operand>, dst: IrOperand, src: X86Operand) {
        match dst {
            IrOperand::Ident(ident) => {
                variables.insert(ident, src);
            }
            _ => panic!("x86: expected identifier as first operand"),
        }
    }

    // Setup enter jump target
    let base_pos = x86.machine_code.len();
    let jt = get_mut_x86_jump_target(x86, bb.enter_label);
    jt.pos = base_pos;

    // Prologue
    // push rbp
    x86.machine_code.push(0xff);
    x86.machine_code.push(modrm(6, reg_id(X86Reg::RBP)));

    // mov rbp rsp
    push_instruction(x86,
                     X86Opcode::MOV,
                     IrType::I64,
                     X86Operand::Register(X86Reg::RBP),
                     X86Operand::Register(X86Reg::RSP));

    // sub rsp x (gets filled in later, if needed)
    let sub_byte_pos = x86.machine_code.len();

    // Function body
    let mut variables = HashMap::new();
    let mut require_stack_frame = false;
    let mut curr_stack_offset = 0isize;
    let num_insns = insns.len();

    for (i, insn) in insns.iter().enumerate() {
        match insn.opcode {
            IrOpcode::Nop => {
                x86.machine_code.push(0x90);
            }

            IrOpcode::Alloca => {
                curr_stack_offset -= size_of_ir_type(insn.ty, x86.addr_size);
                let dst = X86Operand::Stack(curr_stack_offset);
                let src = to_x86_operand(&variables, insn.op2);
                push_instruction(x86, X86Opcode::MOV, insn.ty, dst, src);
                insert_variable(&mut variables, insn.op1, src);
            }

            IrOpcode::Copy => {
                let dst = to_x86_operand(&variables, insn.op1);
                let src = to_x86_operand(&variables, insn.op2);
                push_instruction(x86, X86Opcode::MOV, insn.ty, dst, src);
                insert_variable(&mut variables, insn.op1, src);
            }

            IrOpcode::Add => {
                let lhs = to_x86_operand(&variables, insn.op2);
                let rhs = to_x86_operand(&variables, insn.op3);
                push_instruction(x86, X86Opcode::ADD, insn.ty, lhs, rhs);
                insert_variable(&mut variables, insn.op1, lhs);
            }

            IrOpcode::Call => {
                if let IrOperand::Ident(ident) = insn.op2 {
                    if (ident.symbol == x86.debug_break_symbol) {
                        x86.machine_code.push(0xcc);
                    }
                }

            }


            IrOpcode::Return => {
                // Store return value in RAX
                if let IrOperand::None = insn.op1 {
                } else {
                    let src = to_x86_operand(&variables, insn.op1);
                    if let X86Operand::Register(X86Reg::RAX) = src {
                    } else {
                        push_instruction(x86, X86Opcode::MOV, insn.ty, X86Operand::Register(X86Reg::RAX), src);
                    }
                }

                // Jump to the end of the function
                if i < num_insns - 1 {
                    let pos = x86.machine_code.len();
                    let jt = get_mut_x86_jump_target(x86, bb.exit_label);
                    jt.jumps.push(pos);
                }
            }

            _ => {},
        }
    }

    // Setup exit jump target
    let return_pos = x86.machine_code.len();
    let jt = get_mut_x86_jump_target(x86, bb.exit_label);
    jt.pos = return_pos;

    // Epilogue
    // TODO(alexander) add rsp x

    // pop rbp
    x86.machine_code.push(0x8f);
    x86.machine_code.push(modrm(0, reg_id(X86Reg::RBP)));

    // ret
    x86.machine_code.push(0xc3);
}

fn get_mut_x86_jump_target<'a>(x86: &'a mut X86Assembler, label: IrIdent) -> &'a mut X86JumpTarget {
    match x86.jump_targets.get(&label) {
        Some(_) => {}
        None => {
            let jt = X86JumpTarget {
                label,
                jumps: Vec::new(),
                pos: 0,
            };
            x86.jump_targets.insert(label, jt);
        }
    }

    x86.jump_targets.get_mut(&label).unwrap()
}

/***************************************************************************
 * Machine code builder helpers
 ***************************************************************************/

fn push_instruction(x86: &mut X86Assembler, opcode: X86Opcode, ty: IrType, dst: X86Operand, src: X86Operand) {
    let opcode_offset = match ty {
        IrType::I8 => 1,
        IrType::I64 |
        IrType::U64 => {
            x86.machine_code.push(REX_W);
            0
        },
        _ => 0,
    };
    
    match (dst, src) {
        (X86Operand::Stack(disp1), X86Operand::Stack(disp2)) => {
            let reg = X86Reg::RAX; // TODO(alexander): properly select a free scratch register
            x86.machine_code.push(get_RM_opcode(opcode, opcode_offset));
            x86.machine_code.push(modrm_disp(reg_id(reg), reg_id(X86Reg::RBP), disp2));
            push_displacement(x86, disp2);

            x86.machine_code.push(get_MR_opcode(opcode, opcode_offset));
            x86.machine_code.push(modrm_disp(reg_id(reg), reg_id(X86Reg::RBP), disp1));
            push_displacement(x86, disp1);
        }
        
        (X86Operand::Stack(disp), X86Operand::Register(reg)) => {
            x86.machine_code.push(get_RM_opcode(opcode, opcode_offset));
            x86.machine_code.push(modrm_disp(reg_id(reg), reg_id(X86Reg::RBP), disp));
            push_displacement(x86, disp);
        }

        (X86Operand::Stack(disp), X86Operand::Value(val)) => {
            let (opcode, opcode_reg) = get_MI_opcode(opcode, opcode_offset);
            x86.machine_code.push(opcode);
            x86.machine_code.push(modrm_disp(opcode_reg, reg_id(X86Reg::RBP), disp));
            push_displacement(x86, disp);
            push_immediate(x86, val);
        }

        (X86Operand::Register(reg), X86Operand::Stack(disp)) => {
            x86.machine_code.push(get_RM_opcode(opcode, opcode_offset));
            x86.machine_code.push(modrm_disp(reg_id(reg), reg_id(X86Reg::RBP), disp));
            push_displacement(x86, disp);
        }

        (X86Operand::Register(reg1), X86Operand::Register(reg2)) => {
            x86.machine_code.push(get_RM_opcode(opcode, opcode_offset));
            x86.machine_code.push(modrm(reg_id(reg1), reg_id(reg2)));
        }

        (X86Operand::Register(reg), X86Operand::Value(val)) => {
            let (opcode, opcode_reg) = get_MI_opcode(opcode, opcode_offset);
            x86.machine_code.push(opcode);
            x86.machine_code.push(modrm(opcode_reg, reg_id(reg)));
            push_immediate(x86, val);
        }

        _ => panic!("x86: cannot store to value operand"),
    }
}

fn get_MR_opcode(opcode: X86Opcode, opcode_offset: u8) -> u8{
    match opcode {
        X86Opcode::MOV  => 0x89 - opcode_offset,
        X86Opcode::ADD  => 0x01 - opcode_offset,
        X86Opcode::SUB  => 0x29 - opcode_offset,
        X86Opcode::AND  => 0x20 - opcode_offset,
        X86Opcode::OR   => 0x08 - opcode_offset,
        X86Opcode::XOR  => 0x31 - opcode_offset,
        X86Opcode::CMP  => 0x39 - opcode_offset,
        X86Opcode::TEST => 0x84 - opcode_offset,
        _ => unimplemented!(),
    }
}

fn get_RM_opcode(opcode: X86Opcode, opcode_offset: u8) -> u8 {
    match opcode {
        X86Opcode::MOV  => 0x8b - opcode_offset,
        X86Opcode::ADD  => 0x03 - opcode_offset,
        X86Opcode::SUB  => 0x2b - opcode_offset,
        X86Opcode::AND  => 0x22 - opcode_offset,
        X86Opcode::OR   => 0x0a - opcode_offset,
        X86Opcode::XOR  => 0x33 - opcode_offset,
        X86Opcode::CMP  => 0x3b - opcode_offset,
        X86Opcode::TEST => 0x84 - opcode_offset,
        _ => unimplemented!(),
    }
}

fn get_MI_opcode(opcode: X86Opcode, opcode_offset: u8) -> (u8, u8) {
    match opcode {
        X86Opcode::MOV  => (0xc7 - opcode_offset, 0),
        X86Opcode::ADD  => (0x81 - opcode_offset, 0),
        X86Opcode::SUB  => (0x81 - opcode_offset, 5),
        X86Opcode::AND  => (0x81 - opcode_offset, 4),
        X86Opcode::OR   => (0x81 - opcode_offset, 1),
        X86Opcode::XOR  => (0x81 - opcode_offset, 6),
        X86Opcode::CMP  => (0x81 - opcode_offset, 7),
        X86Opcode::TEST => (0xf6 - opcode_offset, 0),
        _ => unimplemented!(),
    }
}

fn push_displacement(x86: &mut X86Assembler, disp: isize) {
    if disp < -128 && disp > 127 {
        let v = disp as i32;
        x86.machine_code.push((v         & 0xFFi32) as u8);
        x86.machine_code.push(((v >> 8)  & 0xFFi32) as u8);
        x86.machine_code.push(((v >> 16) & 0xFFi32) as u8);
        x86.machine_code.push(((v >> 24) & 0xFFi32) as u8);
    } else {
        let v = disp as i8;
        x86.machine_code.push(v as u8);
    }
}

fn push_immediate(x86: &mut X86Assembler, immediate: X86Value) {
    match immediate {
        X86Value::Int8(v) => {
            x86.machine_code.push(v as u8);
        }
        X86Value::Int32(v) => {
            x86.machine_code.push((v         & 0xFFi32) as u8);
            x86.machine_code.push(((v >> 8)  & 0xFFi32) as u8);
            x86.machine_code.push(((v >> 16) & 0xFFi32) as u8);
            x86.machine_code.push(((v >> 24) & 0xFFi32) as u8);
        }
        X86Value::Int64(v) => {
            x86.machine_code.push((v         & 0xFFi64) as u8);
            x86.machine_code.push(((v >> 8)  & 0xFFi64) as u8);
            x86.machine_code.push(((v >> 16) & 0xFFi64) as u8);
            x86.machine_code.push(((v >> 24) & 0xFFi64) as u8);
            x86.machine_code.push(((v >> 32) & 0xFFi64) as u8);
            x86.machine_code.push(((v >> 40) & 0xFFi64) as u8);
            x86.machine_code.push(((v >> 48) & 0xFFi64) as u8);
            x86.machine_code.push(((v >> 56) & 0xFFi64) as u8);
        }
    }
}

fn reg_id(reg: X86Reg) -> u8 {
    match reg {
        X86Reg::RAX => 0,
        X86Reg::RCX => 1,
        X86Reg::RDX => 2,
        X86Reg::RBX => 3,
        X86Reg::RSP => 4,
        X86Reg::RBP => 5,
        X86Reg::RSI => 6,
        X86Reg::RDI => 7,
        X86Reg::R8  => 0, // NOTE(alexander): set rex_r = 1, only in 64-bit mode
        X86Reg::R9  => 1,
        X86Reg::R10 => 2,
        X86Reg::R11 => 3,
        X86Reg::R12 => 4,
        X86Reg::R13 => 5,
        X86Reg::R14 => 6,
        X86Reg::R15 => 7,
    }
}

const REX:   u8 = 0b01000000;
const REX_W: u8 = 0b01001000;

#[inline]
fn modrm(reg: u8, rm: u8) -> u8 {
    return 0b11000000 | (reg << 3) | rm;
}

#[inline]
fn modrm_disp(reg: u8, rm: u8, disp: isize) -> u8 {
    if disp < -128 && disp > 127 {
        return 0b10000000 | (reg << 3) | rm;
    } else {
        return 0b01000000 | (reg << 3) | rm;
    }
}
