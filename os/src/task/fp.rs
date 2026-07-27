// 这里负责操作系统的上下文切换：按架构保存和恢复用户态浮点寄存器上下文。
#[cfg(target_arch = "riscv64")]
mod arch {
    use core::arch::asm;

    #[derive(Clone, Copy)]
    pub struct UserFloatState {
        regs: [u64; 32],
        fcsr: usize,
        #[cfg(feature = "riscv-vector")]
        vector_regs: [u128; 32],
        #[cfg(feature = "riscv-vector")]
        vl: usize,
        #[cfg(feature = "riscv-vector")]
        vtype: usize,
        #[cfg(feature = "riscv-vector")]
        vstart: usize,
        #[cfg(feature = "riscv-vector")]
        vcsr: usize,
    }

    impl UserFloatState {
        // 创建一份清零的 RISC-V 用户浮点上下文。
        pub const fn new() -> Self {
            Self {
                regs: [0; 32],
                fcsr: 0,
                #[cfg(feature = "riscv-vector")]
                vector_regs: [0; 32],
                #[cfg(feature = "riscv-vector")]
                vl: 0,
                #[cfg(feature = "riscv-vector")]
                vtype: 0,
                #[cfg(feature = "riscv-vector")]
                vstart: 0,
                #[cfg(feature = "riscv-vector")]
                vcsr: 0,
            }
        }

        // 保存当前 CPU 上的用户浮点寄存器和 fcsr。
        // 执行时用 RISC-V 浮点存储指令把 f0-f31 写入数组，并额外读取 fcsr 保存舍入/异常状态。
        pub fn save_current(&mut self) {
            let base = self.regs.as_mut_ptr();
            let fcsr: usize;
            unsafe {
                asm!(
                    "fsd f0,   0({base})",
                    "fsd f1,   8({base})",
                    "fsd f2,  16({base})",
                    "fsd f3,  24({base})",
                    "fsd f4,  32({base})",
                    "fsd f5,  40({base})",
                    "fsd f6,  48({base})",
                    "fsd f7,  56({base})",
                    "fsd f8,  64({base})",
                    "fsd f9,  72({base})",
                    "fsd f10, 80({base})",
                    "fsd f11, 88({base})",
                    "fsd f12, 96({base})",
                    "fsd f13, 104({base})",
                    "fsd f14, 112({base})",
                    "fsd f15, 120({base})",
                    "fsd f16, 128({base})",
                    "fsd f17, 136({base})",
                    "fsd f18, 144({base})",
                    "fsd f19, 152({base})",
                    "fsd f20, 160({base})",
                    "fsd f21, 168({base})",
                    "fsd f22, 176({base})",
                    "fsd f23, 184({base})",
                    "fsd f24, 192({base})",
                    "fsd f25, 200({base})",
                    "fsd f26, 208({base})",
                    "fsd f27, 216({base})",
                    "fsd f28, 224({base})",
                    "fsd f29, 232({base})",
                    "fsd f30, 240({base})",
                    "fsd f31, 248({base})",
                    "csrr {fcsr}, fcsr",
                    base = in(reg) base,
                    fcsr = lateout(reg) fcsr,
                );
            }
            self.fcsr = fcsr;

            #[cfg(feature = "riscv-vector")]
            {
            let vector_base = self.vector_regs.as_mut_ptr();
            let vl: usize;
            let vtype: usize;
            let vstart: usize;
            let vcsr: usize;
            unsafe {
                asm!(
                    ".option push",
                    ".option arch, +v",
                    "csrr {vl}, vl",
                    "csrr {vtype}, vtype",
                    "csrr {vstart}, vstart",
                    "csrr {vcsr}, vcsr",
                    "vsetivli zero, 2, e64, m1, ta, ma",
                    "mv {scratch}, {base}",
                    "vse64.v v0, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v1, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v2, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v3, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v4, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v5, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v6, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v7, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v8, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v9, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v10, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v11, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v12, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v13, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v14, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v15, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v16, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v17, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v18, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v19, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v20, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v21, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v22, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v23, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v24, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v25, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v26, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v27, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v28, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v29, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v30, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vse64.v v31, ({scratch})",
                    ".option pop",
                    base = in(reg) vector_base,
                    scratch = out(reg) _,
                    vl = out(reg) vl,
                    vtype = out(reg) vtype,
                    vstart = out(reg) vstart,
                    vcsr = out(reg) vcsr,
                );
            }
            self.vl = vl;
            self.vtype = vtype;
            self.vstart = vstart;
            self.vcsr = vcsr;
            }
        }

        // 把保存过的用户浮点状态恢复到 CPU。
        // 执行时用 RISC-V 浮点加载指令恢复 f0-f31，并把保存的 fcsr 写回硬件寄存器。
        pub fn restore_current(&self) {
            let base = self.regs.as_ptr();
            let fcsr = self.fcsr;
            unsafe {
                asm!(
                    "fld f0,   0({base})",
                    "fld f1,   8({base})",
                    "fld f2,  16({base})",
                    "fld f3,  24({base})",
                    "fld f4,  32({base})",
                    "fld f5,  40({base})",
                    "fld f6,  48({base})",
                    "fld f7,  56({base})",
                    "fld f8,  64({base})",
                    "fld f9,  72({base})",
                    "fld f10, 80({base})",
                    "fld f11, 88({base})",
                    "fld f12, 96({base})",
                    "fld f13, 104({base})",
                    "fld f14, 112({base})",
                    "fld f15, 120({base})",
                    "fld f16, 128({base})",
                    "fld f17, 136({base})",
                    "fld f18, 144({base})",
                    "fld f19, 152({base})",
                    "fld f20, 160({base})",
                    "fld f21, 168({base})",
                    "fld f22, 176({base})",
                    "fld f23, 184({base})",
                    "fld f24, 192({base})",
                    "fld f25, 200({base})",
                    "fld f26, 208({base})",
                    "fld f27, 216({base})",
                    "fld f28, 224({base})",
                    "fld f29, 232({base})",
                    "fld f30, 240({base})",
                    "fld f31, 248({base})",
                    "csrw fcsr, {fcsr}",
                    base = in(reg) base,
                    fcsr = in(reg) fcsr,
                );
            }

            #[cfg(feature = "riscv-vector")]
            {
            let vector_base = self.vector_regs.as_ptr();
            let vl = self.vl;
            let vtype = self.vtype;
            let vstart = self.vstart;
            let vcsr = self.vcsr;
            unsafe {
                asm!(
                    ".option push",
                    ".option arch, +v",
                    "vsetivli zero, 2, e64, m1, ta, ma",
                    "mv {scratch}, {base}",
                    "vle64.v v0, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v1, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v2, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v3, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v4, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v5, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v6, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v7, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v8, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v9, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v10, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v11, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v12, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v13, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v14, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v15, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v16, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v17, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v18, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v19, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v20, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v21, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v22, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v23, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v24, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v25, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v26, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v27, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v28, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v29, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v30, ({scratch})",
                    "addi {scratch}, {scratch}, 16",
                    "vle64.v v31, ({scratch})",
                    "csrw vcsr, {vcsr}",
                    "vsetvl zero, {vl}, {vtype}",
                    "csrw vstart, {vstart}",
                    ".option pop",
                    base = in(reg) vector_base,
                    scratch = lateout(reg) _,
                    vl = in(reg) vl,
                    vtype = in(reg) vtype,
                    vstart = in(reg) vstart,
                    vcsr = in(reg) vcsr,
                );
            }
            }
        }
    }
}

#[cfg(target_arch = "loongarch64")]
mod arch {
    use core::arch::asm;

    #[derive(Clone, Copy)]
    pub struct UserFloatState {
        regs: [u64; 32],
    }

    impl UserFloatState {
        // 创建一份清零的 LoongArch 用户浮点上下文。
        pub const fn new() -> Self {
            Self { regs: [0; 32] }
        }

        // 保存当前 CPU 上的用户浮点寄存器。
        // 执行时用 LoongArch 浮点存储指令把 $f0-$f31 顺序写入任务保存区。
        pub fn save_current(&mut self) {
            let base = self.regs.as_mut_ptr();
            unsafe {
                asm!(
                    "fst.d $f0,  {base}, 0",
                    "fst.d $f1,  {base}, 8",
                    "fst.d $f2,  {base}, 16",
                    "fst.d $f3,  {base}, 24",
                    "fst.d $f4,  {base}, 32",
                    "fst.d $f5,  {base}, 40",
                    "fst.d $f6,  {base}, 48",
                    "fst.d $f7,  {base}, 56",
                    "fst.d $f8,  {base}, 64",
                    "fst.d $f9,  {base}, 72",
                    "fst.d $f10, {base}, 80",
                    "fst.d $f11, {base}, 88",
                    "fst.d $f12, {base}, 96",
                    "fst.d $f13, {base}, 104",
                    "fst.d $f14, {base}, 112",
                    "fst.d $f15, {base}, 120",
                    "fst.d $f16, {base}, 128",
                    "fst.d $f17, {base}, 136",
                    "fst.d $f18, {base}, 144",
                    "fst.d $f19, {base}, 152",
                    "fst.d $f20, {base}, 160",
                    "fst.d $f21, {base}, 168",
                    "fst.d $f22, {base}, 176",
                    "fst.d $f23, {base}, 184",
                    "fst.d $f24, {base}, 192",
                    "fst.d $f25, {base}, 200",
                    "fst.d $f26, {base}, 208",
                    "fst.d $f27, {base}, 216",
                    "fst.d $f28, {base}, 224",
                    "fst.d $f29, {base}, 232",
                    "fst.d $f30, {base}, 240",
                    "fst.d $f31, {base}, 248",
                    base = in(reg) base,
                );
            }
        }

        // 把保存过的用户浮点寄存器恢复到 CPU。
        // 执行时用 LoongArch 浮点加载指令从任务保存区恢复 $f0-$f31。
        pub fn restore_current(&self) {
            let base = self.regs.as_ptr();
            unsafe {
                asm!(
                    "fld.d $f0,  {base}, 0",
                    "fld.d $f1,  {base}, 8",
                    "fld.d $f2,  {base}, 16",
                    "fld.d $f3,  {base}, 24",
                    "fld.d $f4,  {base}, 32",
                    "fld.d $f5,  {base}, 40",
                    "fld.d $f6,  {base}, 48",
                    "fld.d $f7,  {base}, 56",
                    "fld.d $f8,  {base}, 64",
                    "fld.d $f9,  {base}, 72",
                    "fld.d $f10, {base}, 80",
                    "fld.d $f11, {base}, 88",
                    "fld.d $f12, {base}, 96",
                    "fld.d $f13, {base}, 104",
                    "fld.d $f14, {base}, 112",
                    "fld.d $f15, {base}, 120",
                    "fld.d $f16, {base}, 128",
                    "fld.d $f17, {base}, 136",
                    "fld.d $f18, {base}, 144",
                    "fld.d $f19, {base}, 152",
                    "fld.d $f20, {base}, 160",
                    "fld.d $f21, {base}, 168",
                    "fld.d $f22, {base}, 176",
                    "fld.d $f23, {base}, 184",
                    "fld.d $f24, {base}, 192",
                    "fld.d $f25, {base}, 200",
                    "fld.d $f26, {base}, 208",
                    "fld.d $f27, {base}, 216",
                    "fld.d $f28, {base}, 224",
                    "fld.d $f29, {base}, 232",
                    "fld.d $f30, {base}, 240",
                    "fld.d $f31, {base}, 248",
                    base = in(reg) base,
                );
            }
        }
    }
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
mod arch {
    #[derive(Clone, Copy)]
    pub struct UserFloatState;

    impl UserFloatState {
        // 非目标架构下保留一个空实现，方便本地检查编译。
        pub const fn new() -> Self {
            Self
        }

        // 空架构实现里不需要保存浮点状态。
        // 执行时保持空操作，供非目标架构做本地检查时通过类型接口。
        pub fn save_current(&mut self) {}

        // 空架构实现里不需要恢复浮点状态。
        // 执行时保持空操作，和 save_current 对称。
        pub fn restore_current(&self) {}
    }
}

pub use arch::UserFloatState;
