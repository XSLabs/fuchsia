// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::arch::ArchWidth;
use starnix_uapi::user_address::UserAddress;
use zx::sys::zx_thread_state_general_regs_t;

#[cfg(target_arch = "aarch64")]
use starnix_uapi::user_address::ArchSpecific;

pub struct ThreadStartInfo {
    pub entry: UserAddress,
    pub stack: UserAddress,
    pub environ: UserAddress,
    pub arch_width: ArchWidth,
}

#[cfg(target_arch = "aarch64")]
impl From<ThreadStartInfo> for zx_thread_state_general_regs_t {
    fn from(val: ThreadStartInfo) -> Self {
        if val.arch_width.is_arch32() {
            // Mask in 32-bit mode.
            let mut cpsr: u64 = zx::sys::ZX_REG_CPSR_ARCH_32_MASK;
            // Check if we're starting in thumb.
            if (val.entry.ptr() & 0x1) == 0x1 {
                // TODO(https://fxbug.dev/379669623) Need to have checked the ELF hw cap
                // before this to make sure it's not just misaligned.
                cpsr |= zx::sys::ZX_REG_CPSR_THUMB_MASK;
            }
            let mut reg = zx_thread_state_general_regs_t {
                pc: (val.entry.ptr() & !1) as u64,
                sp: val.stack.ptr() as u64,
                cpsr,
                ..Default::default()
            };
            reg.r[13] = reg.sp;
            reg.r[14] = reg.pc;
            reg.r[0] = reg.sp; // argc
            reg.r[1] = reg.sp + (size_of::<u32>() as u64); // argv
            reg.r[2] = val.environ.ptr() as u64; // envp
            reg
        } else {
            zx_thread_state_general_regs_t {
                pc: val.entry.ptr() as u64,
                sp: val.stack.ptr() as u64,
                ..Default::default()
            }
        }
    }
}

#[cfg(target_arch = "riscv64")]
impl From<ThreadStartInfo> for zx_thread_state_general_regs_t {
    fn from(val: ThreadStartInfo) -> Self {
        zx_thread_state_general_regs_t {
            pc: val.entry.ptr() as u64,
            sp: val.stack.ptr() as u64,
            ..Default::default()
        }
    }
}

#[cfg(target_arch = "x86_64")]
impl From<ThreadStartInfo> for zx_thread_state_general_regs_t {
    fn from(val: ThreadStartInfo) -> Self {
        zx_thread_state_general_regs_t {
            rip: val.entry.ptr() as u64,
            rsp: val.stack.ptr() as u64,
            ..Default::default()
        }
    }
}
