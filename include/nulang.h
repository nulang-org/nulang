/**
 * Nulang stable C embedding API.
 *
 * This header exposes the same ABI as `src/ffi/c_api.rs`. Include it and link
 * against `libnulang.so` (or the static `nulang` rlib) to compile and run
 * Nulang source from C/C++.
 */

#ifndef NULANG_H
#define NULANG_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/** Opaque handle to a Nulang runtime context. */
typedef struct NulangRuntime NulangRuntime;

/** A Nulang value passed by value (raw NaN-boxed bits). */
typedef struct {
    uint64_t raw;
} NulangValue;

/** ABI-stable C type token used with `nulang_register_native_function`. */
typedef enum {
    NULANG_CTYPE_I64 = 0,
    NULANG_CTYPE_F64 = 1,
    NULANG_CTYPE_BOOL = 2,
    NULANG_CTYPE_CSTR = 3,
    NULANG_CTYPE_VOIDPTR = 4,
    NULANG_CTYPE_UNIT = 5,
    NULANG_CTYPE_VALUE = 6,
} NulangCType;

/* -------------------------------------------------------------------------- */
/* Runtime lifecycle                                                           */
/* -------------------------------------------------------------------------- */

/** Create a new Nulang runtime. */
NulangRuntime *nulang_runtime_new(void);

/** Free a runtime created by `nulang_runtime_new`. */
void nulang_runtime_free(NulangRuntime *runtime);

/* -------------------------------------------------------------------------- */
/* Compilation and execution                                                   */
/* -------------------------------------------------------------------------- */

/**
 * Compile Nulang source code.
 *
 * Returns a non-negative module handle on success, or -1 on error. On error,
 * the message is available through `nulang_last_error`.
 */
int64_t nulang_compile(NulangRuntime *runtime, const char *source);

/**
 * Run the top-level expression of a compiled module.
 *
 * On error the result is nil and `nulang_last_error` contains the message.
 */
NulangValue nulang_run(NulangRuntime *runtime, int64_t module_handle);

/**
 * Call an exported Nulang function by name.
 *
 * `args` is an array of `NulangValue` of length `arg_count`; arguments are
 * passed in r0, r1, etc. The function's return value (register 0) is
 * returned. On error the result is nil.
 */
NulangValue nulang_call_function(NulangRuntime *runtime,
                                 int64_t module_handle,
                                 const char *name,
                                 const NulangValue *args,
                                 size_t arg_count);

/* -------------------------------------------------------------------------- */
/* Error handling                                                              */
/* -------------------------------------------------------------------------- */

/** Return the last error message, or NULL if there is none. */
const char *nulang_last_error(NulangRuntime *runtime);

/** Clear the runtime's last error state. */
void nulang_clear_error(NulangRuntime *runtime);

/* -------------------------------------------------------------------------- */
/* Value constructors                                                          */
/* -------------------------------------------------------------------------- */

NulangValue nulang_value_int_new(int64_t value);
NulangValue nulang_value_float_new(double value);
NulangValue nulang_value_bool_new(bool value);
NulangValue nulang_value_nil(void);
NulangValue nulang_value_unit(void);

/**
 * Create a Nulang string value by interning `s` into `module_handle`'s
 * constant pool. Returns nil on error.
 */
NulangValue nulang_module_string(NulangRuntime *runtime,
                                 int64_t module_handle,
                                 const char *s);

/* -------------------------------------------------------------------------- */
/* Value extractors                                                            */
/* -------------------------------------------------------------------------- */

int64_t nulang_value_int(NulangValue value);
double nulang_value_float(NulangValue value);
bool nulang_value_bool(NulangValue value);
bool nulang_value_is_nil(NulangValue value);
bool nulang_value_is_unit(NulangValue value);

/**
 * Return a C string representation of a Nulang value.
 *
 * The returned pointer is owned by the runtime and normally valid until the
 * runtime is freed. Call `nulang_free_string` to release it earlier.
 */
const char *nulang_value_to_string(NulangRuntime *runtime, NulangValue value);

/**
 * Free a C string previously returned by `nulang_value_to_string`.
 *
 * Returns true if the pointer was recognized and freed.
 */
bool nulang_free_string(NulangRuntime *runtime, const char *ptr);

/* -------------------------------------------------------------------------- */
/* Native function registration                                                */
/* -------------------------------------------------------------------------- */

/**
 * Register a native C function so it can be called from Nulang.
 *
 * Use `"__nulang_registered__"` as the library name in the Nulang `extern`
 * block when the function was registered this way. Returns 0 on success,
 * -1 on error.
 */
int nulang_register_native_function(const char *name,
                                    const void *ptr,
                                    const NulangCType *params,
                                    size_t param_count,
                                    NulangCType ret);

#ifdef __cplusplus
}
#endif

#endif /* NULANG_H */
