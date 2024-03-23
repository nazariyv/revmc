//! JIT compiler implementation.

use crate::{Backend, Builder, Bytecode, IntCC, OpcodeData, OpcodeFlags, Result};
use revm_interpreter::{opcode as op, InstructionResult};
use revm_jit_core::{JitEvmFn, OptimizationLevel};
use revm_primitives::{SpecId, U256};
use std::path::PathBuf;

const STACK_CAP: usize = 1024;
// const WORD_SIZE: usize = 32;

// TODO: indexvec or something
type Opcode = usize;

// TODO: cannot find function if `compile` is called a second time

/// JIT compiler for EVM bytecode.
#[derive(Debug)]
pub struct JitEvm<B> {
    backend: B,
    out_dir: Option<PathBuf>,
    config: FcxConfig,
    function_counter: usize,
}

impl<B: Backend + Default> Default for JitEvm<B> {
    fn default() -> Self {
        Self::new(B::default())
    }
}

impl<B: Backend> JitEvm<B> {
    /// Creates a new instance of the JIT compiler with the given backend.
    pub fn new(backend: B) -> Self {
        Self { backend, out_dir: None, config: FcxConfig::default(), function_counter: 0 }
    }

    /// Dumps the IR and potential to the given directory after compilation.
    ///
    /// Disables dumping if `output_dir` is `None`.
    ///
    /// Creates a subdirectory with the name of the backend in the given directory.
    pub fn set_dump_to(&mut self, output_dir: Option<PathBuf>) {
        self.backend.set_is_dumping(output_dir.is_some());
        self.config.comments_enabled = output_dir.is_some();
        self.out_dir = output_dir;
    }

    /// Sets the optimization level.
    ///
    /// Note that some backends may not support setting the optimization level after initialization.
    ///
    /// Defaults to the backend's initial optimization level.
    pub fn set_opt_level(&mut self, level: OptimizationLevel) {
        self.backend.set_opt_level(level);
    }

    /// Sets whether to enable debug assertions.
    ///
    /// These are useful for debugging, but they do incur a small performance penalty.
    ///
    /// Defaults to `cfg!(debug_assertions)`.
    pub fn set_debug_assertions(&mut self, yes: bool) {
        self.backend.set_debug_assertions(yes);
        self.config.debug_assertions = yes;
    }

    /// Sets whether to pass the stack length through the arguments.
    ///
    /// If this is set to `true`, the EVM stack will be passed in the arguments rather than
    /// allocated in the function locally.
    ///
    /// This is useful to inspect the stack after the function has been executed, but it does
    /// incur a performance penalty as the pointer might not be able to be fully optimized.
    ///
    /// Defaults to `false`.
    pub fn set_pass_stack_through_args(&mut self, yes: bool) {
        self.config.stack_through_args = yes;
    }

    /// Sets whether to pass the stack length through the arguments.
    ///
    /// If this is set to `true`, the EVM stack length will be passed in the arguments rather than
    /// allocated in the function locally.
    ///
    /// This is useful to inspect the stack length after the function has been executed, but it does
    /// incur a performance penalty as the pointer might not be able to be fully optimized.
    ///
    /// Defaults to `false`.
    pub fn set_pass_stack_len_through_args(&mut self, yes: bool) {
        self.config.pass_stack_len_through_args = yes;
    }

    /// Sets whether to disable gas accounting.
    ///
    /// Greatly improves compilation speed and performance, at the cost of not being able to check
    /// for gas exhaustion.
    ///
    /// Use with care.
    ///
    /// Defaults to `false`.
    pub fn set_disable_gas(&mut self, disable_gas: bool) {
        self.config.gas_disabled = disable_gas;
    }

    /// Sets whether to store the amount of gas used to the `gas` object.
    ///
    /// Disabling this improves performance by being able to skip some loads/stores.
    ///
    /// Defaults to `true`.
    pub fn set_store_gas_used(&mut self, yes: bool) {
        self.config.store_gas_used = yes;
    }

    /// Sets the static gas limit.
    ///
    /// Improves performance by being able to skip most gas checks.
    ///
    /// Has no effect if `disable_gas` is set, and will make the compiled function ignore the gas
    /// limit argument.
    ///
    /// Defaults to `None`.
    pub fn set_static_gas_limit(&mut self, static_gas_limit: Option<u64>) {
        self.config.static_gas_limit = static_gas_limit;
    }

    /// Compiles the given EVM bytecode into a JIT function.
    #[instrument(level = "debug", skip_all, ret)]
    pub fn compile(&mut self, bytecode: &[u8], spec: SpecId) -> Result<JitEvmFn> {
        let bytecode = debug_time!("parse", || self.parse_bytecode(bytecode, spec))?;
        debug_time!("compile", || self.compile_bytecode(&bytecode))
    }

    /// Frees all functions compiled by this JIT compiler.
    ///
    /// # Safety
    ///
    /// Because this function invalidates any pointers retrived from the corresponding module, it
    /// should only be used when none of the functions from that module are currently executing and
    /// none of the `fn` pointers are called afterwards.
    pub unsafe fn free_all_functions(&mut self) -> Result<()> {
        self.backend.free_all_functions()
    }

    fn parse_bytecode<'a>(&mut self, bytecode: &'a [u8], spec: SpecId) -> Result<Bytecode<'a>> {
        trace!(bytecode = revm_primitives::hex::encode(bytecode));
        let mut bytecode = trace_time!("new bytecode", || Bytecode::new(bytecode, spec));
        trace_time!("analyze", || bytecode.analyze())?;
        Ok(bytecode)
    }

    fn compile_bytecode(&mut self, bytecode: &Bytecode<'_>) -> Result<JitEvmFn> {
        if self.config.debug_assertions {
            let params = &[self.backend.type_ptr(), self.backend.type_ptr_sized_int()];
            self.backend.add_callback_function(
                "__callback_panic",
                None,
                params,
                __callback_panic as usize,
            );
        }

        let name = &self.new_name()[..];
        let bcx = self.backend.build_function(name)?;

        trace_time!("translate", || FunctionCx::translate(bcx, bytecode, &self.config))?;

        let verify = |b: &mut B| trace_time!("verify", || b.verify_function(name));
        if let Some(dir) = &self.out_dir {
            trace_time!("dump unopt IR", || {
                let filename = format!("{name}.unopt.{}", self.backend.ir_extension());
                self.backend.dump_ir(&dir.join(filename))
            })?;

            // Dump IR before verifying for better debugging.
            verify(&mut self.backend)?;

            trace_time!("dump unopt disasm", || {
                let filename = format!("{name}.unopt.s");
                self.backend.dump_disasm(&dir.join(filename))
            })?;
        } else {
            verify(&mut self.backend)?;
        }

        trace_time!("optimize", || self.backend.optimize_function(name))?;

        if let Some(dir) = &self.out_dir {
            trace_time!("dump opt IR", || {
                let filename = format!("{name}.opt.{}", self.backend.ir_extension());
                self.backend.dump_ir(&dir.join(filename))
            })?;

            trace_time!("dump opt disasm", || {
                let filename = format!("{name}.opt.s");
                self.backend.dump_disasm(&dir.join(filename))
            })?;
        }

        trace_time!("finalize", || self.backend.get_function(name).map(JitEvmFn::new))
    }

    fn new_name(&mut self) -> String {
        let name = format!("__evm_bytecode_{}", self.function_counter);
        self.function_counter += 1;
        name
    }
}

#[derive(Clone, Debug)]
struct FcxConfig {
    debug_assertions: bool,
    comments_enabled: bool,
    stack_through_args: bool,
    pass_stack_len_through_args: bool,
    gas_disabled: bool,
    store_gas_used: bool,
    static_gas_limit: Option<u64>,
}

impl Default for FcxConfig {
    fn default() -> Self {
        Self {
            debug_assertions: cfg!(debug_assertions),
            comments_enabled: false,
            stack_through_args: false,
            pass_stack_len_through_args: false,
            gas_disabled: false,
            store_gas_used: true,
            static_gas_limit: None,
        }
    }
}

struct FunctionCx<'a, B: Builder> {
    disable_gas: bool,
    comments_enabled: bool,

    /// The backend's function builder.
    bcx: B,

    // Common types.
    isize_type: B::Type,
    word_type: B::Type,
    return_type: B::Type,

    /// The stack length pointer. `isize`.
    stack_len: B::Value,
    /// The stack pointer. Constant throughout the function, passed in the arguments.
    sp: B::Value,
    /// The amount of gas used. `isize`.
    gas_used: B::Value,
    /// The gas limit. Constant throughout the function, passed in the arguments or set statically.
    gas_limit: B::Value,

    /// The bytecode being translated.
    bytecode: &'a Bytecode<'a>,
    /// All entry blocks for each opcode.
    op_blocks: Vec<B::BasicBlock>,
    /// Opcode information.
    op_infos: &'static [op::OpInfo],
    /// The current opcode being translated.
    ///
    /// Note that `self.op_blocks[current_opcode]` does not necessarily equal the builder's current
    /// block.
    current_opcode: Opcode,
}

impl<'a, B: Builder> FunctionCx<'a, B> {
    fn translate(mut bcx: B, bytecode: &'a Bytecode<'a>, config: &FcxConfig) -> Result<()> {
        // Get common types.
        let isize_type = bcx.type_ptr_sized_int();
        let return_type = bcx.type_int(8);
        let word_type = bcx.type_int(256);

        let zero = bcx.iconst(isize_type, 0);

        // Set up entry block.
        let gas_ty = isize_type;
        let gas_ptr = bcx.fn_param(0);
        if config.debug_assertions {
            pointer_panic_with_bool(
                &mut bcx,
                !config.gas_disabled || config.store_gas_used || config.static_gas_limit.is_none(),
                gas_ptr,
                "gas pointer",
            );
        }

        let gas_used = if config.store_gas_used {
            let one = bcx.iconst(isize_type, 1);
            bcx.gep(gas_ty, gas_ptr, one)
        } else {
            let gas_used = bcx.new_stack_slot(isize_type, "gas_used.addr");
            bcx.stack_store(zero, gas_used);
            bcx.stack_addr(gas_used)
        };
        let gas_limit = if let Some(static_gas_limit) = config.static_gas_limit {
            bcx.iconst(isize_type, static_gas_limit as i64)
        } else {
            bcx.load(isize_type, gas_ptr, "gas_limit")
        };

        let sp_arg = bcx.fn_param(1);
        let sp = if config.stack_through_args {
            sp_arg
        } else {
            let stack_type = bcx.type_array(word_type, STACK_CAP as _);
            let stack_slot = bcx.new_stack_slot(stack_type, "stack.addr");
            bcx.stack_addr(stack_slot)
        };
        if config.debug_assertions {
            pointer_panic_with_bool(&mut bcx, config.stack_through_args, sp_arg, "stack pointer");
        }

        let stack_len_arg = bcx.fn_param(2);
        let stack_len = if config.pass_stack_len_through_args {
            stack_len_arg
        } else {
            let stack_len = bcx.new_stack_slot(isize_type, "len.addr");
            bcx.stack_store(zero, stack_len);
            bcx.stack_addr(stack_len)
        };
        if config.debug_assertions {
            pointer_panic_with_bool(
                &mut bcx,
                config.pass_stack_len_through_args,
                stack_len_arg,
                "stack length pointer",
            );
        }

        // Create all opcode entry blocks.
        let op_blocks: Vec<_> = bytecode
            .opcodes
            .iter()
            .enumerate()
            .map(|(i, opcode)| bcx.create_block(&op_block_name_with(i, *opcode, "")))
            .collect();

        // Branch to the first opcode. The bytecode is guaranteed to have at least one opcode.
        assert!(!op_blocks.is_empty(), "translating empty bytecode");
        bcx.br(op_blocks[0]);

        let op_infos = op::spec_opcode_gas(bytecode.spec);

        let mut fx = FunctionCx {
            comments_enabled: config.comments_enabled,
            disable_gas: config.gas_disabled,

            bcx,
            isize_type,
            word_type,
            return_type,
            stack_len,
            sp,
            gas_used,
            gas_limit,
            bytecode,
            op_blocks,
            op_infos,
            current_opcode: 0,
        };
        for i in 0..bytecode.opcodes.len() {
            fx.current_opcode = i;
            fx.translate_opcode()?;
        }

        Ok(())
    }

    fn translate_opcode(&mut self) -> Result<()> {
        let opcode = self.current_opcode;
        let data = self.bytecode.opcodes[opcode];
        let op_block = self.op_blocks[opcode];
        self.bcx.switch_to_block(op_block);

        let op_byte = data.opcode;

        let branch_to_next_opcode = |this: &mut Self| {
            if let Some(next) = this.op_blocks.get(opcode + 1) {
                this.bcx.br(*next);
            }
        };
        let epilogue = |this: &mut Self| {
            this.bcx.seal_block(op_block);
        };

        // Make sure to run the epilogue before returning.
        macro_rules! goto_return {
            ($comment:literal) => {
                branch_to_next_opcode(self);
                if self.comments_enabled {
                    self.add_comment($comment);
                }
                goto_return!(no_branch);
            };
            (no_branch) => {
                epilogue(self);
                return Ok(());
            };
            (build $ret:expr) => {{
                self.build_return($ret);
                goto_return!(no_branch);
            }};
        }

        if data.flags.contains(OpcodeFlags::DISABLED) {
            goto_return!(build InstructionResult::NotActivated);
        }

        if !self.disable_gas && !data.flags.contains(OpcodeFlags::SKIP_GAS) {
            // TODO: JUMPDEST in gas map is 0 for some reason
            let gas =
                if op_byte == op::JUMPDEST { 1 } else { self.op_infos[op_byte as usize].get_gas() };
            self.gas_cost_imm(gas);
        }

        if data.flags.contains(OpcodeFlags::SKIP_LOGIC) {
            goto_return!("skipped");
        }

        macro_rules! unop {
            ($op:ident) => {{
                let mut a = self.pop();
                a = self.bcx.$op(a);
                self.push_unchecked(a);
            }};
        }

        macro_rules! binop {
            ($op:ident) => {{
                let [a, b] = self.popn();
                let r = self.bcx.$op(a, b);
                self.push_unchecked(r);
            }};
            (@if_not_zero $op:ident $(, $extra_cond:expr)?) => {{
                let [a, b] = self.popn();
                let cond = self.bcx.icmp_imm(IntCC::Equal, b, 0);
                let r = self.bcx.lazy_select(
                    cond,
                    self.word_type,
                    |bcx, block| {
                        bcx.set_cold_block(block);
                        bcx.iconst_256(U256::ZERO)
                    },
                    |bcx, _op_block| {
                        // TODO: segfault ??
                        // $(
                        //     let cond = $extra_cond(bcx, a, b);
                        //     return bcx.lazy_select(
                        //         cond,
                        //         self.word_type,
                        //         |bcx, block| {
                        //             bcx.set_cold_block(block);
                        //             bcx.iconst_256(U256::ZERO)
                        //         },
                        //         |bcx, _block| bcx.$op(a, b),
                        //     );
                        //     #[allow(unreachable_code)]
                        // )?
                        bcx.$op(a, b)
                    },
                );
                self.push_unchecked(r);
            }};
        }

        match data.opcode {
            op::STOP => goto_return!(build InstructionResult::Stop),

            op::ADD => binop!(iadd),
            op::MUL => binop!(imul),
            op::SUB => binop!(isub),
            op::DIV => binop!(@if_not_zero udiv),
            op::SDIV => binop!(@if_not_zero sdiv, |bcx: &mut B, a, b| {
                let min = bcx.iconst_256(I256_MIN);
                let a_is_min = bcx.icmp(IntCC::Equal, a, min);
                let b_is_neg1 = bcx.icmp_imm(IntCC::Equal, b, -1);
                bcx.bitand(a_is_min, b_is_neg1)
            }),
            op::MOD => binop!(@if_not_zero urem),
            op::SMOD => binop!(@if_not_zero srem),
            op::ADDMOD => {
                // TODO
                // let [a, b, c] = self.popn();
            }
            op::MULMOD => {
                // TODO
                // let [a, b, c] = self.popn();
            }
            op::EXP => {
                // TODO
                // let [base, exponent] = self.popn();
                // let r = self.bcx.ipow(base, exponent);
                // self.push_unchecked(r);
            }
            op::SIGNEXTEND => {
                // TODO
                // let [a, b] = self.popn();
                // let r = self.bcx.sign_extend(a, b);
                // self.push_unchecked(r);
            }

            op::LT | op::GT | op::SLT | op::SGT | op::EQ => {
                let cond = match op_byte {
                    op::LT => IntCC::UnsignedLessThan,
                    op::GT => IntCC::UnsignedGreaterThan,
                    op::SLT => IntCC::SignedLessThan,
                    op::SGT => IntCC::SignedGreaterThan,
                    op::EQ => IntCC::Equal,
                    _ => unreachable!(),
                };

                let [a, b] = self.popn();
                let r = self.bcx.icmp(cond, a, b);
                let r = self.bcx.zext(self.word_type, r);
                self.push_unchecked(r);
            }
            op::ISZERO => {
                let a = self.pop();
                let r = self.bcx.icmp_imm(IntCC::Equal, a, 0);
                let r = self.bcx.zext(self.word_type, r);
                self.push_unchecked(r);
            }
            op::AND => binop!(bitand),
            op::OR => binop!(bitor),
            op::XOR => binop!(bitxor),
            op::NOT => unop!(bitnot),
            op::BYTE => {
                // TODO
                // let [index, value] = self.popn();
                // let cond = self.bcx.icmp_imm(IntCC::UnsignedLessThan, value, 32);
                // let zero = self.bcx.iconst_256(U256::ZERO);
                // let r = self.bcx.select(cond, cond, zero);
                // self.push_unchecked(r);
            }
            op::SHL => binop!(ishl),
            op::SHR => binop!(ushr),
            op::SAR => binop!(sshr),

            op::KECCAK256 => {}

            op::ADDRESS => {}
            op::BALANCE => {}
            op::ORIGIN => {}
            op::CALLER => {}
            op::CALLVALUE => {}
            op::CALLDATALOAD => {}
            op::CALLDATASIZE => {}
            op::CALLDATACOPY => {}
            op::CODESIZE => {}
            op::CODECOPY => {}

            op::GASPRICE => {}
            op::EXTCODESIZE => {}
            op::EXTCODECOPY => {}
            op::RETURNDATASIZE => {}
            op::RETURNDATACOPY => {}
            op::EXTCODEHASH => {}
            op::BLOCKHASH => {}
            op::COINBASE => {}
            op::TIMESTAMP => {}
            op::NUMBER => {}
            op::DIFFICULTY => {}
            op::GASLIMIT => {}
            op::CHAINID => {}
            op::SELFBALANCE => {}
            op::BASEFEE => {}
            op::BLOBHASH => {}
            op::BLOBBASEFEE => {}

            op::POP => {
                self.pop();
            }
            op::MLOAD => {}
            op::MSTORE => {}
            op::MSTORE8 => {}
            op::SLOAD => {}
            op::SSTORE => {}
            op::JUMP | op::JUMPI => {
                if data.flags.contains(OpcodeFlags::INVALID_JUMP) {
                    self.build_return(InstructionResult::InvalidJump);
                } else if data.flags.contains(OpcodeFlags::STATIC_JUMP) {
                    let target_opcode = data.data as usize;
                    debug_assert_eq!(
                        self.bytecode.opcodes[target_opcode].opcode,
                        op::JUMPDEST,
                        "is_valid_jump returned true for non-JUMPDEST: ic={target_opcode} -> {}",
                        self.bytecode.opcodes[target_opcode].to_raw()
                    );

                    let target = self.op_blocks[target_opcode];
                    if op_byte == op::JUMPI {
                        let cond_word = self.pop();
                        let cond = self.bcx.icmp_imm(IntCC::NotEqual, cond_word, 0);
                        let next = self.op_blocks[opcode + 1];
                        self.bcx.brif(cond, target, next);
                    } else {
                        self.bcx.br(target);
                    }
                } else {
                    todo!("dynamic jumps");
                }

                goto_return!(no_branch);
            }
            op::PC => {
                let pc = self.bcx.iconst_256(U256::from(data.data));
                self.push(pc);
            }
            op::MSIZE => {}
            op::GAS => {}
            op::JUMPDEST => {
                self.bcx.nop();
            }
            op::TLOAD => {}
            op::TSTORE => {}

            op::PUSH0 => {
                let value = self.bcx.iconst_256(U256::ZERO);
                self.push(value);
            }
            op::PUSH1..=op::PUSH32 => {
                // NOTE: This can be None if the bytecode is invalid.
                let imm = self.bytecode.get_imm_of(data);
                let value = imm.map(U256::from_be_slice).unwrap_or_default();
                let value = self.bcx.iconst_256(value);
                self.push(value);
            }

            op::DUP1..=op::DUP16 => self.dup(op_byte - op::DUP1 + 1),

            op::SWAP1..=op::SWAP16 => self.swap(op_byte - op::SWAP1 + 1),

            op::LOG0..=op::LOG4 => {
                let _n = op_byte - op::LOG0;
            }

            op::CREATE => {}
            op::CALL => {}
            op::CALLCODE => {}
            op::RETURN => {}
            op::DELEGATECALL => {}
            op::CREATE2 => {}
            op::STATICCALL => {}
            op::REVERT => {}
            op::INVALID => goto_return!(build InstructionResult::InvalidFEOpcode),
            op::SELFDESTRUCT => {}

            _ => goto_return!(build InstructionResult::OpcodeNotFound),
        }

        goto_return!("normal exit");
    }

    /// Pushes a 256-bit value onto the stack, checking for stack overflow.
    fn push(&mut self, value: B::Value) {
        self.pushn(&[value]);
    }

    /// Pushes 256-bit values onto the stack, checking for stack overflow.
    fn pushn(&mut self, values: &[B::Value]) {
        debug_assert!(values.len() <= STACK_CAP);

        let len = self.load_len();
        let failure_cond =
            self.bcx.icmp_imm(IntCC::UnsignedGreaterThan, len, (STACK_CAP - values.len()) as i64);
        self.build_failure(failure_cond, InstructionResult::StackOverflow);

        self.pushn_unchecked(values);
    }

    /// Pushes a 256-bit value onto the stack, without checking for stack overflow.
    fn push_unchecked(&mut self, value: B::Value) {
        self.pushn_unchecked(&[value]);
    }

    /// Pushes 256-bit values onto the stack, without checking for stack overflow.
    fn pushn_unchecked(&mut self, values: &[B::Value]) {
        let mut len = self.load_len();
        for &value in values {
            let sp = self.sp_at(len);
            self.bcx.store(value, sp);
            len = self.bcx.iadd_imm(len, 1);
        }
        self.store_len(len);
    }

    /// Removes the topmost element from the stack and returns it.
    fn pop(&mut self) -> B::Value {
        self.popn::<1>()[0]
    }

    /// Removes the topmost `N` elements from the stack and returns them.
    fn popn<const N: usize>(&mut self) -> [B::Value; N] {
        debug_assert_ne!(N, 0);
        debug_assert!(N < 26, "too many pops");

        let mut len = self.load_len();
        let failure_cond = self.bcx.icmp_imm(IntCC::UnsignedLessThan, len, N as i64);
        self.build_failure(failure_cond, InstructionResult::StackUnderflow);

        let ret = std::array::from_fn(|i| {
            len = self.bcx.isub_imm(len, 1);
            let sp = self.sp_at(len);
            let name = b'a' + i as u8;
            self.load_word(sp, core::str::from_utf8(&[name]).unwrap())
        });
        self.store_len(len);
        ret
    }

    /// Duplicates the `n`th value from the top of the stack.
    /// `n` cannot be `0`.
    fn dup(&mut self, n: u8) {
        debug_assert_ne!(n, 0);

        let len = self.load_len();
        let failure_cond = self.bcx.icmp_imm(IntCC::UnsignedLessThan, len, n as i64);
        self.build_failure(failure_cond, InstructionResult::StackUnderflow);

        let sp = self.sp_from_top(len, n as usize);
        let value = self.load_word(sp, &format!("dup{n}"));
        self.push(value);
    }

    /// Swaps the topmost value with the `n`th value from the top.
    fn swap(&mut self, n: u8) {
        debug_assert_ne!(n, 0);

        let len = self.load_len();
        let failure_cond = self.bcx.icmp_imm(IntCC::UnsignedLessThan, len, n as i64);
        self.build_failure(failure_cond, InstructionResult::StackUnderflow);

        // let tmp;
        let tmp = self.bcx.new_stack_slot(self.word_type, "tmp.addr");
        // tmp = a;
        let a_sp = self.sp_from_top(len, n as usize + 1);
        let a = self.load_word(a_sp, "a");
        self.bcx.stack_store(a, tmp);
        // a = b;
        let b_sp = self.sp_from_top(len, 1);
        let b = self.load_word(b_sp, "b");
        self.bcx.store(b, a_sp);
        // b = tmp;
        let tmp = self.bcx.stack_load(self.word_type, tmp, "tmp");
        self.bcx.store(tmp, b_sp);
    }

    /// Loads the word at the given pointer.
    fn load_word(&mut self, ptr: B::Value, name: &str) -> B::Value {
        self.bcx.load(self.word_type, ptr, name)
    }

    /// Loads the stack length.
    fn load_len(&mut self) -> B::Value {
        self.bcx.load(self.isize_type, self.stack_len, "len")
    }

    /// Stores the stack length.
    fn store_len(&mut self, value: B::Value) {
        self.bcx.store(value, self.stack_len);
    }

    /// Returns the stack pointer at `len` (`&stack[len]`).
    fn sp_at(&mut self, len: B::Value) -> B::Value {
        self.bcx.gep(self.word_type, self.sp, len)
    }

    /// Returns the stack pointer at `len` from the top (`&stack[CAPACITY - len]`).
    fn sp_from_top(&mut self, len: B::Value, n: usize) -> B::Value {
        debug_assert_ne!(n, 0);
        let len = self.bcx.isub_imm(len, n as i64);
        self.sp_at(len)
    }

    /// Builds a gas cost deduction for an immediate value.
    fn gas_cost_imm(&mut self, cost: u32) {
        if self.disable_gas || cost == 0 {
            return;
        }
        let value = self.bcx.iconst(self.isize_type, cost as i64);
        self.gas_cost(value);
    }

    /// Builds a gas cost deduction for a value.
    fn gas_cost(&mut self, cost: B::Value) {
        if self.disable_gas {
            return;
        }

        let gas_used = self.bcx.load(self.isize_type, self.gas_used, "gas_used");
        let added = self.bcx.iadd(gas_used, cost);
        let failure_cond = self.bcx.icmp(IntCC::UnsignedLessThan, self.gas_limit, added);
        self.build_failure(failure_cond, InstructionResult::OutOfGas);
        self.bcx.store(added, self.gas_used);
    }

    /// `if failure_cond { return ret } else { ... }`
    fn build_failure(&mut self, failure_cond: B::Value, ret: InstructionResult) {
        let failure = self.create_block_after_current("fail");
        let target = self.create_block_after(failure, "contd");
        self.bcx.brif(failure_cond, failure, target);

        self.bcx.set_cold_block(failure);
        self.bcx.switch_to_block(failure);
        self.build_return(ret);

        self.bcx.switch_to_block(target);
    }

    /// Builds `return ret`.
    fn build_return(&mut self, ret: InstructionResult) {
        let old_block = self.bcx.current_block();
        let ret = self.bcx.iconst(self.return_type, ret as i64);
        self.bcx.ret(&[ret]);
        if self.comments_enabled {
            self.add_comment(&format!("return {ret:?}"));
        }
        if let Some(old_block) = old_block {
            self.bcx.seal_block(old_block);
        }
    }

    /// Adds a comment to the current instruction.
    fn add_comment(&mut self, comment: &str) {
        self.bcx.add_comment_to_current_inst(comment);
    }

    /// Creates a named block.
    #[allow(dead_code)]
    fn create_block(&mut self, name: &str) -> B::BasicBlock {
        let name = self.op_block_name(name);
        self.bcx.create_block(&name)
    }

    /// Creates a named block after the given block.
    fn create_block_after(&mut self, after: B::BasicBlock, name: &str) -> B::BasicBlock {
        let name = self.op_block_name(name);
        self.bcx.create_block_after(after, &name)
    }

    /// Creates a named block after the current block.
    fn create_block_after_current(&mut self, name: &str) -> B::BasicBlock {
        let after = self.bcx.current_block().unwrap();
        self.create_block_after(after, name)
    }

    /*
    /// Creates a named block after the current opcode's opcode.
    fn create_block_after_op(&mut self, name: &str) -> B::BasicBlock {
        let after = self.op_blocks[self.current_opcode];
        self.create_block_after(after, name)
    }
    */

    /// Returns the block name for the current opcode with the given suffix.
    fn op_block_name(&self, name: &str) -> String {
        op_block_name_with(self.current_opcode, self.bytecode.opcodes[self.current_opcode], name)
    }
}

fn op_block_name_with(op: Opcode, data: OpcodeData, with: &str) -> String {
    let data = data.to_raw();
    if with.is_empty() {
        format!("op.{op}.{data}")
    } else {
        format!("op.{op}.{data}.{with}")
    }
}

// Pointer must not be null if `set` is true, else it must be null.
fn pointer_panic_with_bool<B: Builder>(bcx: &mut B, must_be_set: bool, ptr: B::Value, name: &str) {
    let panic_cond = if must_be_set { bcx.is_null(ptr) } else { bcx.is_not_null(ptr) };
    let msg = format!(
        "revm_jit panic: {name} must {not}be null due to configuration",
        not = if must_be_set { "not " } else { "" }
    );
    build_panic(bcx, panic_cond, &msg);
}

fn build_panic<B: Builder>(bcx: &mut B, cond: B::Value, msg: &str) {
    let failure = bcx.create_block("panic");
    let target = bcx.create_block("contd");
    bcx.brif(cond, failure, target);

    bcx.set_cold_block(failure);
    bcx.switch_to_block(failure);
    bcx.panic(msg);

    bcx.switch_to_block(target);
}

/* ------------------------------------------ Callbacks ----------------------------------------- */

extern "C" fn __callback_panic(ptr: *const u8, len: usize) -> ! {
    let msg = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len)) };
    panic!("{msg}");
}

#[cfg(test)]
#[allow(dead_code, unused_imports)]
mod tests {
    use super::*;
    use crate::*;
    use interpreter::{opcode::OpInfo, Gas};
    use revm_interpreter::opcode as op;
    use revm_primitives::ruint::uint;

    const DEFAULT_SPEC: SpecId = SpecId::CANCUN;
    const DEFAULT_SPEC_OP_INFO: &[OpInfo; 256] = op::spec_opcode_gas(DEFAULT_SPEC);
    const GAS_MASK: u32 = 0x1FFFFFFF;

    #[inline]
    const fn get_gas(op_info: OpInfo) -> u64 {
        (unsafe { std::mem::transmute::<OpInfo, u32>(op_info) } & GAS_MASK) as u64
    }

    #[cfg(feature = "llvm")]
    #[test]
    fn test_llvm_unopt() {
        test_llvm(OptimizationLevel::None)
    }

    #[cfg(feature = "llvm")]
    #[test]
    fn test_llvm_opt() {
        test_llvm(OptimizationLevel::Aggressive)
    }

    #[cfg(feature = "llvm")]
    fn test_llvm(opt_level: OptimizationLevel) {
        let context = revm_jit_llvm::inkwell::context::Context::create();
        run_tests_with_backend(|| crate::JitEvmLlvmBackend::new(&context, opt_level).unwrap());
    }

    struct TestCase<'a> {
        name: &'a str,
        bytecode: &'a [u8],

        expected_return: InstructionResult,
        expected_stack: &'a [U256],
        expected_gas: u64,
    }

    const fn bytecode_unop(op: u8, a: U256) -> [u8; 36] {
        let mut code = [0; 36];

        let mut i = 0;

        code[i] = op::PUSH32;
        i += 1;
        {
            let mut j = 0;
            let bytes = a.to_be_bytes::<32>();
            while j < 32 {
                code[i] = bytes[j];
                i += 1;
                j += 1;
            }
        }

        code[i] = op;
        // i += 1;

        code
    }

    const fn bytecode_binop(op: u8, a: U256, b: U256) -> [u8; 68] {
        // NOTE: push `b` first.

        let mut code = [0; 68];

        let mut i = 0;

        code[i] = op::PUSH32;
        i += 1;
        {
            let mut j = 0;
            let bytes = b.to_be_bytes::<32>();
            while j < 32 {
                code[i] = bytes[j];
                i += 1;
                j += 1;
            }
        }

        code[i] = op::PUSH32;
        i += 1;
        {
            let mut j = 0;
            let bytes = a.to_be_bytes::<32>();
            while j < 32 {
                code[i] = bytes[j];
                i += 1;
                j += 1;
            }
        }

        code[i] = op;
        // i += 1;

        code
    }

    macro_rules! testcases {
        (@bytecode $op:expr, $a:expr) => { bytecode_unop($op, $a) };
        (@bytecode $op:expr, $a:expr, $b:expr) => { bytecode_binop($op, $a, $b) };

        (@gas $op:expr, $a:expr) => { 3 };
        (@gas $op:expr, $a:expr, $b:expr) => { 6 };

        ($($name:ident($op:expr, $a:expr $(, $b:expr)? => $ret:expr)),* $(,)?) => {
            uint!([$(TestCase {
                name: stringify!($name),
                bytecode: &testcases!(@bytecode $op, $a $(, $b)?),
                expected_return: InstructionResult::Stop,
                expected_stack: &[$ret],
                expected_gas: testcases!(@gas $op, $a $(, $b)?) + get_gas(DEFAULT_SPEC_OP_INFO[$op as usize]),
            }),*])
        };
    }

    static RETURN_CASES: &[TestCase<'static>] = &[
        TestCase {
            name: "empty",
            bytecode: &[],
            expected_return: InstructionResult::Stop,
            expected_stack: &[],
            expected_gas: 0,
        },
        TestCase {
            name: "no stop",
            bytecode: &[op::PUSH0],
            expected_return: InstructionResult::Stop,
            expected_stack: &[U256::ZERO],
            expected_gas: 2,
        },
        TestCase {
            name: "stop",
            bytecode: &[op::STOP],
            expected_return: InstructionResult::Stop,
            expected_stack: &[],
            expected_gas: 0,
        },
        TestCase {
            name: "invalid",
            bytecode: &[op::INVALID],
            expected_return: InstructionResult::InvalidFEOpcode,
            expected_stack: &[],
            expected_gas: 0,
        },
        TestCase {
            name: "unknown",
            bytecode: &[0x21],
            expected_return: InstructionResult::OpcodeNotFound,
            expected_stack: &[],
            expected_gas: 0,
        },
        TestCase {
            name: "underflow1",
            bytecode: &[op::ADD],
            expected_return: InstructionResult::StackUnderflow,
            expected_stack: &[],
            expected_gas: 3,
        },
        TestCase {
            name: "underflow2",
            bytecode: &[op::PUSH0, op::ADD],
            expected_return: InstructionResult::StackUnderflow,
            expected_stack: &[U256::ZERO],
            expected_gas: 5,
        },
    ];

    static CF_CASES: &[TestCase<'static>] = &[
        TestCase {
            name: "basic jump",
            bytecode: &[op::PUSH1, 3, op::JUMP, op::JUMPDEST],
            expected_return: InstructionResult::Stop,
            expected_stack: &[],
            expected_gas: 3 + 8 + 1,
        },
        TestCase {
            name: "unmodified stack after push-jump",
            bytecode: &[op::PUSH1, 3, op::JUMP, op::JUMPDEST, op::PUSH0, op::ADD],
            expected_return: InstructionResult::StackUnderflow,
            expected_stack: &[U256::ZERO],
            expected_gas: 3 + 8 + 1 + 2 + 3,
        },
        TestCase {
            name: "basic jump if",
            bytecode: &[op::PUSH1, 1, op::PUSH1, 5, op::JUMPI, op::JUMPDEST],
            expected_return: InstructionResult::Stop,
            expected_stack: &[],
            expected_gas: 3 + 3 + 10 + 1,
        },
        TestCase {
            name: "unmodified stack after push-jumpif",
            bytecode: &[op::PUSH1, 1, op::PUSH1, 5, op::JUMPI, op::JUMPDEST, op::PUSH0, op::ADD],
            expected_return: InstructionResult::StackUnderflow,
            expected_stack: &[U256::ZERO],
            expected_gas: 3 + 3 + 10 + 1 + 2 + 3,
        },
        TestCase {
            name: "basic loop",
            #[rustfmt::skip]
            bytecode: &[
                op::PUSH1, 3,  // i=3
                op::JUMPDEST,  // i
                op::PUSH1, 1,  // 1, i
                op::SWAP1,     // i, 1
                op::SUB,       // i-1
                op::DUP1,      // i-1, i-1
                op::PUSH1, 2,  // dst, i-1, i-1
                op::JUMPI,     // i=i-1
                op::POP,       //
                op::PUSH1, 69, // 69
            ],
            expected_return: InstructionResult::Stop,
            expected_stack: &[uint!(69_U256)],
            expected_gas: 3 + (1 + 3 + 3 + 3 + 3 + 3 + 10) * 3 + 2 + 3,
        },
    ];

    static ARITH_CASES: &[TestCase<'static>] = &testcases![
        add1(op::ADD, 0_U256, 0_U256 => 0_U256),
        add2(op::ADD, 1_U256, 2_U256 => 3_U256),
        add3(op::ADD, 255_U256, 255_U256 => 510_U256),
        add4(op::ADD, U256::MAX, 1_U256 => 0_U256),
        add5(op::ADD, U256::MAX, 2_U256 => 1_U256),

        sub1(op::SUB, 3_U256, 2_U256 => 1_U256),
        sub2(op::SUB, 1_U256, 2_U256 => MINUS_1),
        sub2(op::SUB, 1_U256, 3_U256 => MINUS_1.wrapping_sub(1_U256)),
        sub3(op::SUB, 255_U256, 255_U256 => 0_U256),

        mul1(op::MUL, 1_U256, 2_U256 => 2_U256),
        mul2(op::MUL, 32_U256, 32_U256 => 1024_U256),
        mul3(op::MUL, U256::MAX, 2_U256 => U256::MAX.wrapping_sub(1_U256)),

        div1(op::DIV, 32_U256, 32_U256 => 1_U256),
        div2(op::DIV, 1_U256, 2_U256 => 0_U256),
        div3(op::DIV, 2_U256, 2_U256 => 1_U256),
        div4(op::DIV, 3_U256, 2_U256 => 1_U256),
        div5(op::DIV, 4_U256, 2_U256 => 2_U256),
        div_by_zero(op::DIV, 32_U256, 0_U256 => 0_U256),

        rem1(op::MOD, 32_U256, 32_U256 => 0_U256),
        rem2(op::MOD, 1_U256, 2_U256 => 1_U256),
        rem3(op::MOD, 2_U256, 2_U256 => 0_U256),
        rem4(op::MOD, 3_U256, 2_U256 => 1_U256),
        rem5(op::MOD, 4_U256, 2_U256 => 0_U256),
        rem_by_zero(op::MOD, 32_U256, 0_U256 => 0_U256),

        sdiv1(op::SDIV, 32_U256, 32_U256 => 1_U256),
        sdiv2(op::SDIV, 1_U256, 2_U256 => 0_U256),
        sdiv3(op::SDIV, 2_U256, 2_U256 => 1_U256),
        sdiv4(op::SDIV, 3_U256, 2_U256 => 1_U256),
        sdiv5(op::SDIV, 4_U256, 2_U256 => 2_U256),
        sdiv_by_zero(op::SDIV, 32_U256, 0_U256 => 0_U256),
        sdiv_min_by_1(op::SDIV, I256_MIN, 1_U256 => I256_MIN.wrapping_neg()),
        // TODO:
        // sdiv_min_by_minus_1(op::SDIV, I256_MIN, MINUS_1 => I256_MIN),
        sdiv_max1(op::SDIV, I256_MAX, 1_U256 => I256_MAX),
        sdiv_max2(op::SDIV, I256_MAX, MINUS_1 => I256_MAX.wrapping_neg()),

        srem1(op::SMOD, 32_U256, 32_U256 => 0_U256),
        srem2(op::SMOD, 1_U256, 2_U256 => 1_U256),
        srem3(op::SMOD, 2_U256, 2_U256 => 0_U256),
        srem4(op::SMOD, 3_U256, 2_U256 => 1_U256),
        srem5(op::SMOD, 4_U256, 2_U256 => 0_U256),
        srem_by_zero(op::SMOD, 32_U256, 0_U256 => 0_U256),

        // TODO:
        // ADDMOD
        // MULMOD
        // EXP
        // SIGNEXTEND
    ];

    static CMP_CASES: &[TestCase<'static>] = &testcases![
        lt1(op::LT, 1_U256, 2_U256 => 1_U256),
        lt2(op::LT, 2_U256, 1_U256 => 0_U256),
        lt3(op::LT, 1_U256, 1_U256 => 0_U256),
        lt4(op::LT, MINUS_1, 1_U256 => 0_U256),

        gt1(op::GT, 1_U256, 2_U256 => 0_U256),
        gt2(op::GT, 2_U256, 1_U256 => 1_U256),
        gt3(op::GT, 1_U256, 1_U256 => 0_U256),
        gt4(op::GT, MINUS_1, 1_U256 => 1_U256),

        slt1(op::SLT, 1_U256, 2_U256 => 1_U256),
        slt2(op::SLT, 2_U256, 1_U256 => 0_U256),
        slt3(op::SLT, 1_U256, 1_U256 => 0_U256),
        slt4(op::SLT, MINUS_1, 1_U256 => 1_U256),

        sgt1(op::SGT, 1_U256, 2_U256 => 0_U256),
        sgt2(op::SGT, 2_U256, 1_U256 => 1_U256),
        sgt3(op::SGT, 1_U256, 1_U256 => 0_U256),
        sgt4(op::SGT, MINUS_1, 1_U256 => 0_U256),

        eq1(op::EQ, 1_U256, 2_U256 => 0_U256),
        eq2(op::EQ, 2_U256, 1_U256 => 0_U256),
        eq3(op::EQ, 1_U256, 1_U256 => 1_U256),

        iszero1(op::ISZERO, 0_U256 => 1_U256),
        iszero2(op::ISZERO, 1_U256 => 0_U256),
        iszero3(op::ISZERO, 2_U256 => 0_U256),
    ];

    static BITWISE_CASES: &[TestCase<'static>] = &testcases![
        and1(op::AND, 0_U256, 0_U256 => 0_U256),
        and2(op::AND, 1_U256, 1_U256 => 1_U256),
        and3(op::AND, 1_U256, 2_U256 => 0_U256),
        and4(op::AND, 255_U256, 255_U256 => 255_U256),

        or1(op::OR, 0_U256, 0_U256 => 0_U256),
        or2(op::OR, 1_U256, 2_U256 => 3_U256),
        or3(op::OR, 1_U256, 3_U256 => 3_U256),
        or4(op::OR, 2_U256, 2_U256 => 2_U256),

        xor1(op::XOR, 0_U256, 0_U256 => 0_U256),
        xor2(op::XOR, 1_U256, 2_U256 => 3_U256),
        xor3(op::XOR, 1_U256, 3_U256 => 2_U256),
        xor4(op::XOR, 2_U256, 2_U256 => 0_U256),

        not1(op::NOT, 0_U256 => U256::MAX),
        not2(op::NOT, U256::MAX => 0_U256),
        not3(op::NOT, 1_U256 => U256::MAX.wrapping_sub(1_U256)),

        // TODO
        // byte1(op::BYTE, 0_U256, 0_U256 => 0_U256),
        // byte2(op::BYTE, 0_U256, 1_U256 => 0_U256),
        // byte3(op::BYTE, 0_U256, 2_U256 => 0_U256),

        shl1(op::SHL, 1_U256, 0_U256 => 1_U256),
        shl2(op::SHL, 1_U256, 1_U256 => 2_U256),
        shl3(op::SHL, 1_U256, 2_U256 => 4_U256),

        shr1(op::SHR, 1_U256, 0_U256 => 1_U256),
        shr2(op::SHR, 2_U256, 1_U256 => 1_U256),
        shr3(op::SHR, 4_U256, 2_U256 => 1_U256),

        sar1(op::SAR, 1_U256, 0_U256 => 1_U256),
        sar2(op::SAR, 2_U256, 1_U256 => 1_U256),
        sar3(op::SAR, 4_U256, 2_U256 => 1_U256),
        sar4(op::SAR, MINUS_1, 1_U256 => MINUS_1),
        sar5(op::SAR, MINUS_1, 2_U256 => MINUS_1),
    ];

    static ALL_TEST_CASES: &[(&str, &[TestCase<'static>])] = &[
        ("return_values", RETURN_CASES),
        ("control_flow", CF_CASES),
        ("arithmetic", ARITH_CASES),
        ("comparison", CMP_CASES),
        ("bitwise", BITWISE_CASES),
    ];

    // TODO: Have to create a new backend per call for now
    fn run_tests_with_backend<B: Backend>(make_backend: impl Fn() -> B) {
        let backend_name = std::any::type_name::<B>().split("::").last().unwrap();
        for &(group_name, cases) in ALL_TEST_CASES {
            println!("Running test group `{group_name}` for backend `{backend_name}`");
            run_test_group(&make_backend, cases);
            println!();
        }
        println!("Running fibonacci tests for backend `{backend_name}`");
        run_fibonacci_tests(make_backend);
    }

    fn run_test_group<B: Backend>(make_backend: impl Fn() -> B, cases: &[TestCase<'_>]) {
        for (i, &TestCase { name, bytecode, expected_return, expected_stack, expected_gas }) in
            cases.iter().enumerate()
        {
            let mut jit = JitEvm::new(make_backend());
            jit.set_pass_stack_through_args(true);
            jit.set_pass_stack_len_through_args(true);

            println!("Running test case {i:2}: {name}");
            println!("  bytecode: {}", format_bytecode(bytecode));
            let f = jit.compile(bytecode, DEFAULT_SPEC).unwrap();

            let mut stack = EvmStack::new();
            let mut stack_len = 0;
            let mut gas = Gas::new(10000);
            let actual_return =
                unsafe { f.call(Some(&mut gas), Some(&mut stack), Some(&mut stack_len)) };
            assert_eq!(actual_return, expected_return);
            assert_eq!(gas.spend(), expected_gas);

            let actual_stack =
                stack.as_slice().iter().take(stack_len).map(|x| x.to_u256()).collect::<Vec<_>>();
            assert_eq!(actual_stack, *expected_stack);
        }
    }

    fn run_fibonacci_tests<B: Backend>(make_backend: impl Fn() -> B) {
        for i in 0..=10 {
            run_fibonacci_test(&make_backend, i);
        }
        run_fibonacci_test(make_backend, 100);

        fn run_fibonacci_test<B: Backend>(make_backend: impl Fn() -> B, input: u16) {
            println!("  Running fibonacci({input}) statically");
            run_fibonacci(&make_backend, input, false);
            println!("  Running fibonacci({input}) dynamically");
            run_fibonacci(make_backend, input, true);
        }

        fn run_fibonacci<B: Backend>(make_backend: impl Fn() -> B, input: u16, dynamic: bool) {
            let code = mk_fibonacci_code(input, dynamic);

            let mut jit = JitEvm::new(make_backend());
            jit.set_pass_stack_through_args(true);
            jit.set_pass_stack_len_through_args(true);
            let f = jit.compile(&code, DEFAULT_SPEC).unwrap();

            let mut gas = Gas::new(10000);
            let mut stack_buf = EvmStack::new_heap();
            let stack = EvmStack::from_mut_vec(&mut stack_buf);
            if dynamic {
                stack.as_mut_slice()[0] = U256::from(input).into();
            }
            let mut stack_len = 0;
            if dynamic {
                stack_len = 1;
            }

            let r = unsafe { f.call(Some(&mut gas), Some(stack), Some(&mut stack_len)) };
            assert_eq!(r, InstructionResult::Stop);
            // Apparently the code does `fibonacci(input - 1)`.
            assert_eq!(stack.as_slice()[0].to_u256(), fibonacci_rust(input + 1));
            assert_eq!(stack_len, 1);
        }

        #[rustfmt::skip]
        fn mk_fibonacci_code(input: u16, dynamic: bool) -> Vec<u8> {
            if dynamic {
                [&[op::JUMPDEST; 3][..], FIBONACCI_CODE].concat()
            } else {
                let input = input.to_be_bytes();
                [[op::PUSH2].as_slice(), input.as_slice(), FIBONACCI_CODE].concat()
            }
        }

        // Modified from jitevm: https://github.com/paradigmxyz/jitevm/blob/f82261fc8a1a6c1a3d40025a910ba0ce3fcaed71/src/test_data.rs#L3
        #[rustfmt::skip]
        const FIBONACCI_CODE: &[u8] = &[
            // 1st/2nd fib number
            op::PUSH1, 0,
            op::PUSH1, 1,
            // 7

            // MAINLOOP:
            op::JUMPDEST,
            op::DUP3,
            op::ISZERO,
            op::PUSH1, 28, // cleanup
            op::JUMPI,

            // fib step
            op::DUP2,
            op::DUP2,
            op::ADD,
            op::SWAP2,
            op::POP,
            op::SWAP1,
            // 19

            // decrement fib step counter
            op::SWAP2,
            op::PUSH1, 1,
            op::SWAP1,
            op::SUB,
            op::SWAP2,
            op::PUSH1, 7, // goto MAINLOOP
            op::JUMP,
            // 28

            // CLEANUP:
            op::JUMPDEST,
            op::SWAP2,
            op::POP,
            op::POP,
            // done: requested fib number is the only element on the stack!
            op::STOP,
        ];
    }

    fn fibonacci_rust(n: u16) -> U256 {
        let mut a = U256::from(0);
        let mut b = U256::from(1);
        for _ in 0..n {
            let tmp = a;
            a = b;
            b = b.wrapping_add(tmp);
        }
        a
    }

    #[test]
    fn test_fibonacci_rust() {
        uint! {
            assert_eq!(fibonacci_rust(0), 0_U256);
            assert_eq!(fibonacci_rust(1), 1_U256);
            assert_eq!(fibonacci_rust(2), 1_U256);
            assert_eq!(fibonacci_rust(3), 2_U256);
            assert_eq!(fibonacci_rust(4), 3_U256);
            assert_eq!(fibonacci_rust(5), 5_U256);
            assert_eq!(fibonacci_rust(6), 8_U256);
            assert_eq!(fibonacci_rust(7), 13_U256);
            assert_eq!(fibonacci_rust(8), 21_U256);
            assert_eq!(fibonacci_rust(9), 34_U256);
            assert_eq!(fibonacci_rust(10), 55_U256);
            assert_eq!(fibonacci_rust(100), 354224848179261915075_U256);
            assert_eq!(fibonacci_rust(1000), 0x2e3510283c1d60b00930b7e8803c312b4c8e6d5286805fc70b594dc75cc0604b_U256);
        }
    }
}
