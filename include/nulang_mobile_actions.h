#ifndef NULANG_MOBILE_ACTIONS_H
#define NULANG_MOBILE_ACTIONS_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Opaque interpreter-only runtime for compiler-authorized native client
 * actions. This is intentionally separate from the generic NulangRuntime API:
 * only action IDs present in the mobile .nbc allowlist are executable here.
 */
typedef struct NulangMobileActionRuntime NulangMobileActionRuntime;

/**
 * Create an action runtime from .nbc bytes.
 *
 * Construction returns an object even when the artifact is invalid so the
 * caller can inspect nulang_mobile_action_runtime_last_error(). Use
 * nulang_mobile_action_runtime_is_ready() before invoking actions.
 *
 * The input bytes are copied/decoded during this call and need not remain
 * alive afterward.
 */
NulangMobileActionRuntime *nulang_mobile_action_runtime_new(
    const uint8_t *bytes,
    size_t len);

/** Return true when the artifact and mobile action metadata decoded cleanly. */
bool nulang_mobile_action_runtime_is_ready(
    const NulangMobileActionRuntime *runtime);

/**
 * Invoke one canonical nulang-ui-msg/1 InvokeAction request.
 *
 * Returns runtime-owned canonical nulang-ui-msg/1 snapshot/patch JSON, or NULL
 * on failure. The returned pointer remains valid until the next invocation on
 * this runtime or until the runtime is freed. Calls on one runtime instance
 * must be serialized.
 */
const char *nulang_mobile_action_runtime_invoke(
    NulangMobileActionRuntime *runtime,
    const char *request_json);

/**
 * Return the most recent constructor/invocation error, or NULL when none is
 * present. The returned pointer is runtime-owned.
 */
const char *nulang_mobile_action_runtime_last_error(
    const NulangMobileActionRuntime *runtime);

/** Free an action runtime. NULL is accepted as a no-op. */
void nulang_mobile_action_runtime_free(NulangMobileActionRuntime *runtime);

#ifdef __cplusplus
}
#endif

#endif /* NULANG_MOBILE_ACTIONS_H */
