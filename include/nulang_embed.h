#ifndef NULANG_EMBED_H
#define NULANG_EMBED_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct NulangRuntime NulangRuntime;

typedef struct NulangValue {
    uint64_t raw;
} NulangValue;

/* Keep discriminants synchronized with src/ffi/marshal.rs::CType. */
typedef enum NulangCType {
    NULANG_CTYPE_I64 = 0,
    NULANG_CTYPE_F64 = 1,
    NULANG_CTYPE_BOOL = 2,
    NULANG_CTYPE_CSTR = 3,
    NULANG_CTYPE_VOID_PTR = 4,
    NULANG_CTYPE_UNIT = 5,
    NULANG_CTYPE_VALUE = 6
} NulangCType;

NulangRuntime *nulang_runtime_new(void);
NulangRuntime *nulang_runtime_new_interpreter(void);
void nulang_runtime_free(NulangRuntime *runtime);

int64_t nulang_compile(NulangRuntime *runtime, const char *source);
int64_t nulang_load_nbc(
    NulangRuntime *runtime,
    const uint8_t *bytes,
    size_t len
);

NulangValue nulang_run(NulangRuntime *runtime, int64_t module_handle);
NulangValue nulang_call_function(
    NulangRuntime *runtime,
    int64_t module_handle,
    const char *name,
    const NulangValue *args,
    size_t arg_count
);

void nulang_clear_error(NulangRuntime *runtime);
const char *nulang_last_error(NulangRuntime *runtime);

int64_t nulang_value_int(NulangValue value);
double nulang_value_float(NulangValue value);
bool nulang_value_bool(NulangValue value);
bool nulang_value_is_nil(NulangValue value);
bool nulang_value_is_unit(NulangValue value);

NulangValue nulang_value_int_new(int64_t value);
NulangValue nulang_value_float_new(double value);
NulangValue nulang_value_bool_new(bool value);
NulangValue nulang_value_nil(void);
NulangValue nulang_value_unit(void);
NulangValue nulang_module_string(
    NulangRuntime *runtime,
    int64_t module_handle,
    const char *s
);

const char *nulang_value_to_string(
    NulangRuntime *runtime,
    NulangValue value
);
bool nulang_free_string(NulangRuntime *runtime, const char *ptr);

int32_t nulang_register_native_function(
    const char *name,
    const void *ptr,
    const NulangCType *params,
    size_t param_count,
    NulangCType ret
);

#ifdef __cplusplus
}
#endif

#endif /* NULANG_EMBED_H */
