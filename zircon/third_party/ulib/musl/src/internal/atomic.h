#pragma once

#include <stdatomic.h>

// TODO(kulakowski) This is a temporary shim to separate the
// bespoke=>C11 atomic conversion from the rewrite of the two
// different CAS styles (return bool and pointer out vs. return old
// value).
static inline int a_cas_shim(_Atomic(int) * p, int t, int s) {
  atomic_compare_exchange_strong(p, &t, s);
  return t;
}

#if defined(__x86_64__)
static inline void a_spin(void) { __asm__ __volatile__("pause" : : : "memory"); }
#elif defined(__aarch64__)
static inline void a_spin(void) { __asm__ __volatile__("yield" : : : "memory"); }
#elif defined(__riscv)
static inline void a_spin(void) { __asm__ __volatile__("pause" : : : "memory"); }
#else
#error "Unknown architecture"
#endif
