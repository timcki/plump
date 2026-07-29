// stack-measurement utilities
// system stats are emitted via log::info! in the scheduler

const STACK_PAINT_WORD: u32 = 0xDEAD_BEEF;

const STACK_GUARD_SKIP: usize = 256;

pub fn paint_stack() {
    #[cfg(target_arch = "riscv32")]
    {
        let sp: usize;
        unsafe {
            core::arch::asm!("mv {}, sp", out(reg) sp);
        }

        unsafe extern "C" {
            static _stack_end_cpu0: u8;
        }
        let bottom = (&raw const _stack_end_cpu0) as usize;

        let paint_bottom = bottom + STACK_GUARD_SKIP;
        let paint_top = sp.saturating_sub(STACK_GUARD_SKIP);

        if paint_top <= paint_bottom {
            return;
        }

        let start = (paint_bottom + 3) & !3;

        let mut addr = start;
        while addr + 4 <= paint_top {
            unsafe {
                core::ptr::write_volatile(addr as *mut u32, STACK_PAINT_WORD);
            }
            addr += 4;
        }
    }
}

pub fn free_stack_bytes() -> usize {
    #[cfg(target_arch = "riscv32")]
    {
        let sp: usize;
        unsafe {
            core::arch::asm!("mv {}, sp", out(reg) sp);
        }

        unsafe extern "C" {
            static _stack_end_cpu0: u8;
        }
        let stack_bottom = (&raw const _stack_end_cpu0) as usize;
        sp.saturating_sub(stack_bottom)
    }

    #[cfg(not(target_arch = "riscv32"))]
    {
        0
    }
}

/// Result of a full canary scan of the stack region.
///
/// `hwm` is the classic high-water mark: distance from the stack top to
/// the lowest non-canary word. `intact_above` counts canary bytes that
/// survive above that lowest break; a genuinely deep call chain leaves
/// (almost) none, while a stray write deep into the stack leaves a
/// large intact span and means `hwm` overstates real usage.
#[derive(Clone, Copy, Default)]
pub struct StackHwmDetail {
    pub hwm: usize,
    pub break_addr: usize,
    pub intact_above: usize,
}

pub fn stack_hwm_detail() -> StackHwmDetail {
    #[cfg(target_arch = "riscv32")]
    {
        unsafe extern "C" {
            static _stack_end_cpu0: u8;
            static _stack_start_cpu0: u8;
        }
        let bottom = (&raw const _stack_end_cpu0) as usize;
        let top = (&raw const _stack_start_cpu0) as usize;

        let scan_bottom = bottom + STACK_GUARD_SKIP;
        let start = (scan_bottom + 3) & !3;

        let mut addr = start;
        let mut break_addr = 0usize;
        let mut intact_above = 0usize;
        while addr + 4 <= top {
            let val = unsafe { core::ptr::read_volatile(addr as *const u32) };
            if val != STACK_PAINT_WORD {
                if break_addr == 0 {
                    break_addr = addr;
                }
            } else if break_addr != 0 {
                intact_above += 4;
            }
            addr += 4;
        }

        StackHwmDetail {
            hwm: if break_addr == 0 {
                0
            } else {
                top.saturating_sub(break_addr)
            },
            break_addr,
            intact_above,
        }
    }

    #[cfg(not(target_arch = "riscv32"))]
    {
        StackHwmDetail::default()
    }
}
