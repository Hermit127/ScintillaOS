#[cfg(target_arch = "riscv64")]
mod arch {
    use core::arch::asm;

    #[derive(Clone, Copy)]
    pub struct UserFloatState {
        regs: [u64; 32],
        fcsr: usize,
    }

    impl UserFloatState {
        pub const fn new() -> Self {
            Self {
                regs: [0; 32],
                fcsr: 0,
            }
        }

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
        }

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
        pub const fn new() -> Self {
            Self { regs: [0; 32] }
        }

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
        pub const fn new() -> Self {
            Self
        }

        pub fn save_current(&mut self) {}

        pub fn restore_current(&self) {}
    }
}

pub use arch::UserFloatState;
