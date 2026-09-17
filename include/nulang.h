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

typedef struct NulangRuntime NulangRuntime;

typedef struct {
    uint64_t raw;
} NulangValue;

typedef enum {
    NULANG_STATUS_OK = 0,
    NULANG_STATUS_INVALID_ARGUMENT = 1,
    NULANG_STATUS_COMPILE_ERROR = 2,
    NULANG_STATUS_RUNTIME_ERROR = 3,
} NulangStatus;

typedef enum {
    NULANG_CTYPE_I64 = 0,
    NULANG_CTYPE_F64 = 1,
    NULANG_CTYPE_BOOL = 2,
    NULANG_CTYPE_CSTR = 3,
    NULANG_CTYPE_VOIDPTR = 4,
    NULANG_CTYPE_UNIT = 5,
    NULANG_CTYPE_VALUE = 6,
} NulangCType;

NulangRuntime *nulang_runtime_new(void);
void nulang_runtime_free(NulangRuntime *runtime);

/**
 * Protected APIs return an explicit status and write results through an
 * out-parameter. A successful Nulang `nil` therefore remains distinguishable
 * from failure.
 */
NulangStatus nulang_compile_protected(NulangRuntime *runtime,
                                      const char *source,
                                      int64_t *out_handle);
NulangStatus nulang_run_protected(NulangRuntime *runtime,
                                  int64_t module_handle,
                                  NulangValue *out_value);
NulangStatus nulang_call_function_protected(NulangRuntime *runtime,
                                            int64_t module_handle,
                                            const char *name,
                                            const NulangValue *args,
                                            size_t arg_count,
                                            NulangValue *out_value);

/** Legacy sentinel-return APIs retained for compatibility. */
int64_t nulang_compile(NulangRuntime *runtime, const char *source);
NulangValue nulang_run(NulangRuntime *runtime, int64_t module_handle);
NulangValue nulang_call_function(NulangRuntime *runtime,
                                 int64_t module_handle,
                                 const char *name,
                                 const NulangValue *args,
                                 size_t arg_count);

const char *nulang_last_error(NulangRuntime *runtime);
void nulang_clear_error(NulangRuntime *runtime);

NulangValue nulang_value_int_new(int64_t value);
NulangValue nulang_value_float_new(double value);
NulangValue nulang_value_bool_new(bool value);
NulangValue nulang_value_nil(void);
NulangValue nulang_value_unit(void);
NulangValue nulang_module_string(NulangRuntime *runtime,
                                 int64_t module_handle,
                                 const char *s);

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

int nulang_runtime_register_native_function(NulangRuntime *runtime,
                                            const char *name,
                                            const void *ptr,
                                            const NulangCType *params,
                                            size_t param_count,
                                            NulangCType ret);
int nulang_register_native_function(const char *name,
                                    const void *ptr,
                                    const NulangCType *params,
                                    size_t param_count,
                                    NulangCType ret);

#ifdef __cplusplus
}
#endif

#endif /* NULANG_H */
