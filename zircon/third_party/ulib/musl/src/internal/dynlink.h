#ifndef ZIRCON_THIRD_PARTY_ULIB_MUSL_SRC_INTERNAL_DYNLINK_H_
#define ZIRCON_THIRD_PARTY_ULIB_MUSL_SRC_INTERNAL_DYNLINK_H_

#include <elf.h>
#include <features.h>
#include <link.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/sanitizer.h>

#include "libc.h"

__BEGIN_CDECLS

typedef Elf64_Ehdr Ehdr;
typedef Elf64_Phdr Phdr;
typedef Elf64_Sym Sym;
#define R_TYPE(x) ((x) & 0x7fffffff)
#define R_SYM(x) ((x) >> 32)
#define R_INFO ELF64_R_INFO

/* These enum constants provide unmatchable default values for
 * any relocation type the arch does not use. */
enum {
  REL_NONE = 0,
  REL_SYMBOLIC = -100,
  REL_GOT,
  REL_PLT,
  REL_RELATIVE,
  REL_OFFSET,
  REL_OFFSET32,
  REL_COPY,
  REL_DTPMOD,
  REL_DTPOFF,
  REL_TPOFF,
  REL_TPOFF_NEG,
  REL_TLSDESC,
  REL_FUNCDESC,
  REL_FUNCDESC_VAL,
};

#include "reloc.h"

#ifndef DT_DEBUG_INDIRECT
#define DT_DEBUG_INDIRECT 0
#endif

// This is the return value of the dynamic linker startup functions.
// They return all the way back to _start so as to pop their stack
// frames.  The DL_START_ASM code at _start then receives these two
// values and jumps to the entry point with the argument in place for
// the C ABI and return address/frame pointer cleared so it's the base
// of the call stack.
#ifndef DL_START_RETURN
typedef struct {
  void* arg;
  void* entry;
} dl_start_return_t;
#define DL_START_RETURN(entry, arg) \
  (dl_start_return_t) { (arg), (entry) }
#endif

dl_start_return_t _dl_start(void* start_arg, void* vdso) ATTR_LIBC_VISIBILITY;
dl_start_return_t __dls2(void* start_arg, void* vdso) ATTR_LIBC_VISIBILITY;

// We can access these with simple PC-relative relocs.
// Both of these symbols are defined automagically by the linker.
// Since we use a standard 0-based DSO layout, __ehdr_start matches
// the lowest address in the DSO image.
extern const ElfW(Ehdr) __ehdr_start[] ATTR_LIBC_VISIBILITY;
extern ElfW(Dyn) _DYNAMIC[] ATTR_LIBC_VISIBILITY;

void _dl_log_unlogged(void) ATTR_LIBC_VISIBILITY;
void _dl_log_write(const char* buffer, size_t len) ATTR_LIBC_VISIBILITY;
void _dl_log_write_init(zx_handle_t handle, uint32_t info) ATTR_LIBC_VISIBILITY;
void _dl_log_write_init_fallback(void) ATTR_LIBC_VISIBILITY;

void _dl_rdlock(void) ATTR_LIBC_VISIBILITY;
void _dl_unlock(void) ATTR_LIBC_VISIBILITY;

void _dl_locked_report_globals(sanitizer_memory_snapshot_callback_t* callback,
                               void* callback_arg) ATTR_LIBC_VISIBILITY;

void _dl_phdr_report_globals(sanitizer_memory_snapshot_callback_t* callback, void* callback_arg,
                             size_t load_bias, const Phdr* phdrs,
                             size_t phnum) ATTR_LIBC_VISIBILITY;

void _dl_iterate_loaded_libs(void) ATTR_LIBC_VISIBILITY;

__END_CDECLS

#endif  // ZIRCON_THIRD_PARTY_ULIB_MUSL_SRC_INTERNAL_DYNLINK_H_
