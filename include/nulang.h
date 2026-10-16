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
 * Convert a Nulang value to a C string using the module that owns its string
 * constant pool. Prefer this function whenever converting a String value.
 *
 * The returned pointer is owned by the runtime until released via
 * `nulang_free_string` or until the runtime is destroyed. Returns NULL and
 * sets `nulang_last_error` when the module handle or string id is invalid.
 *
 * The caller must supply the module handle that produced the value. Since
 * NulangValue currently contains raw tagged bits, the ABI cannot verify
 * provenance if two different modules use the same string id.
 */
const char *nulang_value_to_string_in_module(NulangRuntime *runtime,
                                              int64_t module_handle,
                                              NulangValue value);

/**
 * Legacy conversion for primitive values and unambiguous string values.
 *
 * If more than one compiled module maps the string id to different text (or
 * does not contain that id), returns NULL with an ambiguity error instead of
 * silently interpreting it using the most recently compiled module.
 * Prefer `nulang_value_to_string_in_module` for strings.
 *
 * The returned pointer is runtime-owned and can be released early using
 * `nulang_free_string`.
 */
const char *nulang_value_to_string(NulangRuntime *runtime, NulangValue value);

/**
 * Free a C string previously returned by `nulang_value_to_string`.
 *
 * Returns true if the pointer was recognized and freed.
 */
bool nulang_free_string(NulangRuntime *runtime, const char *ptr);

/* -------------------------------------------------------------------------- */
/* Opaque runtime-owned values (preferred embedding API)                     */
/* -------------------------------------------------------------------------- */

/**
 * Runtime-owned value identifier (0 is invalid).
 *
 * Values are copied into the owning runtime and survive later module calls.
 * Handles are unique across runtime instances and are never reused.
 * Release each handle before destroying the runtime.
 */
typedef uint64_t NulangOwnedValueHandle;

/** Capture a legacy tagged value with explicit module provenance. */
NulangOwnedValueHandle nulang_owned_value_from_module(NulangRuntime *runtime,
                                                      int64_t module_handle,
                                                      NulangValue raw);

/** Run a module and return a runtime-owned value handle (0 on error). */
NulangOwnedValueHandle nulang_run_owned(NulangRuntime *runtime,
                                       int64_t module_handle);

/** Create an integer or UTF-8 C-string owned by the given runtime. */
NulangOwnedValueHandle nulang_owned_value_int_new(NulangRuntime *runtime,
                                                  int64_t value);
NulangOwnedValueHandle nulang_owned_value_string_new(NulangRuntime *runtime,
                                                     const char *value);

/** Invoke an exported function using only handles from this runtime. */
NulangOwnedValueHandle nulang_call_function_owned(
    NulangRuntime *runtime,
    int64_t module_handle,
    const char *name,
    const NulangOwnedValueHandle *args,
    size_t arg_count);

/** Extract an owned integer (false if stale, foreign-runtime or wrong type). */
bool nulang_owned_value_int(NulangRuntime *runtime,
                            NulangOwnedValueHandle handle,
                            int64_t *out);

/**
 * Convert an owned value into a runtime-managed C string.
 * Call nulang_free_string to release its buffer. Returns NULL on error.
 * Strings containing interior NUL cannot be represented in this interface.
 */
const char *nulang_owned_value_to_string(NulangRuntime *runtime,
                                         NulangOwnedValueHandle handle);

/** Release a value handle (false when stale, unknown, or wrong runtime). */
bool nulang_owned_value_release(NulangRuntime *runtime,
                                NulangOwnedValueHandle handle);

/* -------------------------------------------------------------------------- */
/* Native function registration                                                */
/* -------------------------------------------------------------------------- */

/**
 * Register a native C callback for this embedding runtime only.
 *
 * The function pointer and its code must remain valid until runtime teardown.
 * The signature types must match the pointed-to C function exactly.
 * Use extern "__nulang_registered__" { ... } in Nulang code.
 *
 * Separate NulangRuntime instances have separate callback namespaces.
 * Returns 0 on success, or -1 and nulang_last_error on failure.
 */
int nulang_runtime_register_native_function(NulangRuntime *runtime,
                                            const char *name,
                                            const void *ptr,
                                            const NulangCType *params,
                                            size_t param_count,
                                            NulangCType ret);

/**
 * Legacy process-global registration for standalone VMs.
 *
 * Embedded NulangRuntime instances use their own registry and do not consult
 * the process-global callbacks. Use nulang_runtime_register_native_function
 * when embedding.
 *
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
