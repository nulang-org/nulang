#ifndef NULANG_MOBILE_HOST_H
#define NULANG_MOBILE_HOST_H

#include <stddef.h>
#include <stdint.h>

#include "nulang_embed.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct NulangMobileApp NulangMobileApp;

typedef void (*NulangMobileJsonCallback)(
    const char *json,
    void *context
);

typedef struct NulangMobileCallbacks {
    NulangMobileJsonCallback document;
    NulangMobileJsonCallback message;
    void *context;
} NulangMobileCallbacks;

typedef enum NulangMobileStatus {
    NULANG_MOBILE_OK = 0,
    NULANG_MOBILE_INVALID_ARGUMENT = 1,
    NULANG_MOBILE_ALREADY_ACTIVE = 2,
    NULANG_MOBILE_RUNTIME_ERROR = 3,
    NULANG_MOBILE_CALLBACK_ERROR = 4,
    NULANG_MOBILE_ARTIFACT_ERROR = 5,
    NULANG_MOBILE_RUN_ERROR = 6,
    NULANG_MOBILE_ALLOCATION_ERROR = 7
} NulangMobileStatus;

/*
 * Create one interpreter-only mobile runtime, register the semantic UI
 * callbacks, and load a frozen .nbc artifact.
 *
 * The current native-function registry is process-global. Therefore v1
 * intentionally permits only one active NulangMobileApp per process.
 *
 * On failure, *out_app is NULL. `error` may be NULL when error_len == 0.
 */
NulangMobileStatus nulang_mobile_app_new(
    const uint8_t *nbc,
    size_t nbc_len,
    NulangMobileCallbacks callbacks,
    NulangMobileApp **out_app,
    char *error,
    size_t error_len
);

/* Execute the loaded module. UI callbacks may fire synchronously during run. */
NulangMobileStatus nulang_mobile_app_run(
    NulangMobileApp *app,
    NulangValue *out_value,
    char *error,
    size_t error_len
);

/* Free the runtime and release the process-global bridge slot. */
void nulang_mobile_app_free(NulangMobileApp *app);

#ifdef __cplusplus
}
#endif

#endif /* NULANG_MOBILE_HOST_H */
