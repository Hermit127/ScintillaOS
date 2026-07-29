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

    #[repr(C, align(16))]
    #[derive(Clone, Copy)]
    pub struct UserFloatState {
        regs: [u128; 32],
    }

    impl UserFloatState {
        pub const fn new() -> Self {
            Self { regs: [0; 32] }
        }

        pub fn save_current(&mut self) {
            unsafe { save_lsx(self.regs.as_mut_ptr()) }
        }

        pub fn restore_current(&self) {
            unsafe { restore_lsx(self.regs.as_ptr()) }
        }
    }

    #[target_feature(enable = "lsx")]
    unsafe fn save_lsx(base: *mut u128) {
        asm!(
            "vst $vr0,  {base}, 0",
            "vst $vr1,  {base}, 16",
            "vst $vr2,  {base}, 32",
            "vst $vr3,  {base}, 48",
            "vst $vr4,  {base}, 64",
            "vst $vr5,  {base}, 80",
            "vst $vr6,  {base}, 96",
            "vst $vr7,  {base}, 112",
            "vst $vr8,  {base}, 128",
            "vst $vr9,  {base}, 144",
            "vst $vr10, {base}, 160",
            "vst $vr11, {base}, 176",
            "vst $vr12, {base}, 192",
            "vst $vr13, {base}, 208",
            "vst $vr14, {base}, 224",
            "vst $vr15, {base}, 240",
            "vst $vr16, {base}, 256",
            "vst $vr17, {base}, 272",
            "vst $vr18, {base}, 288",
            "vst $vr19, {base}, 304",
            "vst $vr20, {base}, 320",
            "vst $vr21, {base}, 336",
            "vst $vr22, {base}, 352",
            "vst $vr23, {base}, 368",
            "vst $vr24, {base}, 384",
            "vst $vr25, {base}, 400",
            "vst $vr26, {base}, 416",
            "vst $vr27, {base}, 432",
            "vst $vr28, {base}, 448",
            "vst $vr29, {base}, 464",
            "vst $vr30, {base}, 480",
            "vst $vr31, {base}, 496",
            base = in(reg) base,
            options(nostack),
        );
    }

    #[target_feature(enable = "lsx")]
    unsafe fn restore_lsx(base: *const u128) {
        asm!(
            "vld $vr0,  {base}, 0",
            "vld $vr1,  {base}, 16",
            "vld $vr2,  {base}, 32",
            "vld $vr3,  {base}, 48",
            "vld $vr4,  {base}, 64",
            "vld $vr5,  {base}, 80",
            "vld $vr6,  {base}, 96",
            "vld $vr7,  {base}, 112",
            "vld $vr8,  {base}, 128",
            "vld $vr9,  {base}, 144",
            "vld $vr10, {base}, 160",
            "vld $vr11, {base}, 176",
            "vld $vr12, {base}, 192",
            "vld $vr13, {base}, 208",
            "vld $vr14, {base}, 224",
            "vld $vr15, {base}, 240",
            "vld $vr16, {base}, 256",
            "vld $vr17, {base}, 272",
            "vld $vr18, {base}, 288",
            "vld $vr19, {base}, 304",
            "vld $vr20, {base}, 320",
            "vld $vr21, {base}, 336",
            "vld $vr22, {base}, 352",
            "vld $vr23, {base}, 368",
            "vld $vr24, {base}, 384",
            "vld $vr25, {base}, 400",
            "vld $vr26, {base}, 416",
            "vld $vr27, {base}, 432",
            "vld $vr28, {base}, 448",
            "vld $vr29, {base}, 464",
            "vld $vr30, {base}, 480",
            "vld $vr31, {base}, 496",
            base = in(reg) base,
            options(nostack),
        );
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
