#ifndef NULANG_MOBILE_HOST_H
#define NULANG_MOBILE_HOST_H

#include <stddef.h>
#include <stdint.h>

#include "nulang_embed.h"
#include "nulang_mobile_actions.h"

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
    NULANG_MOBILE_ALLOCATION_ERROR = 7,
    NULANG_MOBILE_ACTION_ERROR = 8
} NulangMobileStatus;

/*
 * Create one interpreter-only mobile runtime, register the semantic UI
 * callbacks, load a frozen .nbc artifact, and construct the restricted
 * client-action runtime from the same artifact bytes.
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

/*
 * Execute one compiler-authorized canonical nulang-ui-msg/1 InvokeAction.
 *
 * On success, *out_result_json points to runtime-owned canonical
 * nulang-ui-msg/1 snapshot/patch JSON. The caller must copy it before the next
 * action invocation or before freeing the app. Calls must remain serialized
 * with the other NulangMobileApp lifecycle methods.
 */
NulangMobileStatus nulang_mobile_app_invoke_action(
    NulangMobileApp *app,
    const char *request_json,
    const char **out_result_json,
    char *error,
    size_t error_len
);

/* Free both runtimes and release the process-global bridge slot. */
void nulang_mobile_app_free(NulangMobileApp *app);

#ifdef __cplusplus
}
#endif

#endif /* NULANG_MOBILE_HOST_H */
