// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>
#include <zircon/tls.h>

#include "asm-linkage.h"
#include "../jmp_buf.h"


jmp_buf.llvm_libc_function setjmp

  // Move the jmp_buf pointer to a temporary register.
  // We'll use x0 as a scratch register since we clobber it on return anyway.
  mov x16, x0

  // Load the manglers into temporary registers.  We'll ensure that by return
  // no registers contain the raw manglers, so those values never leak.
  adr_global x0, LIBC_ASM_LINKAGE(gJmpBufManglers)
  ldp x1, x2, [x0, #8*JB_PC]
  ldp x3, x4, [x0, #8*JB_FP]
  ldr x5, [x0, #8*JB_X(18)]

  // Get the thread pointer, where the unsafe SP is stored.
  mrs x0, TPIDR_EL0
  ldr x0, [x0, #ZX_TLS_UNSAFE_SP_OFFSET]

  // Store all the vanilla callee-saves registers.
  stp x19, x20, [x16, #8*JB_X(19)]
  stp x21, x22, [x16, #8*JB_X(21)]
  stp x23, x24, [x16, #8*JB_X(23)]
  stp x25, x26, [x16, #8*JB_X(25)]
  stp x27, x28, [x16, #8*JB_X(27)]
  stp  d8,  d9, [x16, #8*JB_D(8)]
  stp d10, d11, [x16, #8*JB_D(10)]
  stp d12, d13, [x16, #8*JB_D(12)]
  stp d14, d15, [x16, #8*JB_D(14)]

  eor x5, x5, x18      // Mangled shadow call stack pointer.
  eor x4, x4, x0       // Mangled unsafe SP.
  eor x1, x1, x30      // Mangled PC (LR).
  eor x3, x3, x29      // Mangled FP.
  mov x0, sp
  eor x2, x2, x0       // Mangled SP.

.ifne JB_SP - JB_PC - 1
.error "JB_SP expected to follow JB_PC immediately"
.endif
  stp x1, x2, [x16, #8*JB_PC]
.ifne JB_USP - JB_FP - 1
.error "JB_USP expected to follow JB_FP immediately"
.endif
  stp x3, x4, [x16, #8*JB_FP]
  str x5, [x16, #8*JB_X(18)]

  mov w0, wzr
  ret

.end_function
