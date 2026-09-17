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

/**
 * A Nulang value passed by value.
 *
 * String-valued instances are module-scoped: their payload is an interned
 * string index in the module that produced or created them.
 */
typedef struct {
    uint64_t raw;
} NulangValue;

/** ABI-stable C type token used with native function registration. */
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

NulangRuntime *nulang_runtime_new(void);
void nulang_runtime_free(NulangRuntime *runtime);

/* -------------------------------------------------------------------------- */
/* Compilation and execution                                                   */
/* -------------------------------------------------------------------------- */

int64_t nulang_compile(NulangRuntime *runtime, const char *source);
NulangValue nulang_run(NulangRuntime *runtime, int64_t module_handle);
NulangValue nulang_call_function(NulangRuntime *runtime,
                                 int64_t module_handle,
                                 const char *name,
                                 const NulangValue *args,
                                 size_t arg_count);

/* -------------------------------------------------------------------------- */
/* Error handling                                                              */
/* -------------------------------------------------------------------------- */

const char *nulang_last_error(NulangRuntime *runtime);
void nulang_clear_error(NulangRuntime *runtime);

/* -------------------------------------------------------------------------- */
/* Value constructors                                                          */
/* -------------------------------------------------------------------------- */

NulangValue nulang_value_int_new(int64_t value);
NulangValue nulang_value_float_new(double value);
NulangValue nulang_value_bool_new(bool value);
NulangValue nulang_value_nil(void);
NulangValue nulang_value_unit(void);
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
const char *nulang_value_to_string(NulangRuntime *runtime, NulangValue value);
const char *nulang_module_value_to_string(NulangRuntime *runtime,
                                          int64_t module_handle,
                                          NulangValue value);
bool nulang_free_string(NulangRuntime *runtime, const char *ptr);

/* -------------------------------------------------------------------------- */
/* Native function registration                                                */
/* -------------------------------------------------------------------------- */

/**
 * Register a native callback for one NulangRuntime.
 *
 * Library-less `extern { ... }` declarations compiled by this runtime use the
 * private binding first. A same-named callback registered through the legacy
 * process-global API remains only a fallback.
 */
int nulang_runtime_register_native_function(NulangRuntime *runtime,
                                            const char *name,
                                            const void *ptr,
                                            const NulangCType *params,
                                            size_t param_count,
                                            NulangCType ret);

/**
 * Legacy process-global registration. Prefer
 * `nulang_runtime_register_native_function` for embedded applications.
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
