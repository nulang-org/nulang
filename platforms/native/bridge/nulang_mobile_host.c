#include "nulang_mobile_host.h"

#include <stdlib.h>
#include <string.h>

struct NulangMobileApp {
    NulangRuntime *runtime;
    int64_t module_handle;
};

/*
 * Nulang's pre-registered native-function registry is currently process-global.
 * Keep one bridge slot until registrations become runtime-scoped.
 *
 * The v1 host contract requires lifecycle calls to be serialized by the
 * platform wrapper. Swift/Kotlin hosts already run runtime work on one worker.
 */
static NulangMobileApp *g_active_app = NULL;
static NulangMobileCallbacks g_callbacks = {0};

static void copy_error(char *out, size_t out_len, const char *message) {
    if (out == NULL || out_len == 0) {
        return;
    }

    if (message == NULL) {
        message = "unknown Nulang mobile host error";
    }

    size_t n = strlen(message);
    if (n >= out_len) {
        n = out_len - 1;
    }
    memcpy(out, message, n);
    out[n] = '\0';
}

static void bridge_document(const char *json) {
    if (g_callbacks.document != NULL) {
        g_callbacks.document(json, g_callbacks.context);
    }
}

static void bridge_message(const char *json) {
    if (g_callbacks.message != NULL) {
        g_callbacks.message(json, g_callbacks.context);
    }
}

static int register_ui_callbacks(void) {
    static const NulangCType string_arg[] = {NULANG_CTYPE_CSTR};

    if (nulang_register_native_function(
            "nulang_ui_document",
            (const void *)bridge_document,
            string_arg,
            1,
            NULANG_CTYPE_UNIT) != 0) {
        return -1;
    }

    if (nulang_register_native_function(
            "nulang_ui_message",
            (const void *)bridge_message,
            string_arg,
            1,
            NULANG_CTYPE_UNIT) != 0) {
        return -1;
    }

    return 0;
}

NulangMobileStatus nulang_mobile_app_new(
    const uint8_t *nbc,
    size_t nbc_len,
    NulangMobileCallbacks callbacks,
    NulangMobileApp **out_app,
    char *error,
    size_t error_len
) {
    if (out_app == NULL || nbc == NULL || nbc_len == 0) {
        copy_error(error, error_len, "invalid mobile app arguments");
        return NULANG_MOBILE_INVALID_ARGUMENT;
    }
    *out_app = NULL;

    if (g_active_app != NULL) {
        copy_error(
            error,
            error_len,
            "only one Nulang mobile app may be active per process"
        );
        return NULANG_MOBILE_ALREADY_ACTIVE;
    }

    NulangRuntime *runtime = nulang_runtime_new_interpreter();
    if (runtime == NULL) {
        copy_error(error, error_len, "failed to create interpreter runtime");
        return NULANG_MOBILE_RUNTIME_ERROR;
    }

    if (register_ui_callbacks() != 0) {
        nulang_runtime_free(runtime);
        copy_error(error, error_len, "failed to register native UI callbacks");
        return NULANG_MOBILE_CALLBACK_ERROR;
    }

    int64_t module_handle = nulang_load_nbc(runtime, nbc, nbc_len);
    if (module_handle < 0) {
        const char *runtime_error = nulang_last_error(runtime);
        copy_error(error, error_len, runtime_error);
        nulang_runtime_free(runtime);
        return NULANG_MOBILE_ARTIFACT_ERROR;
    }

    NulangMobileApp *app = (NulangMobileApp *)calloc(1, sizeof(*app));
    if (app == NULL) {
        nulang_runtime_free(runtime);
        copy_error(error, error_len, "failed to allocate mobile app handle");
        return NULANG_MOBILE_ALLOCATION_ERROR;
    }

    app->runtime = runtime;
    app->module_handle = module_handle;
    g_callbacks = callbacks;
    g_active_app = app;
    *out_app = app;
    copy_error(error, error_len, "");
    return NULANG_MOBILE_OK;
}

NulangMobileStatus nulang_mobile_app_run(
    NulangMobileApp *app,
    NulangValue *out_value,
    char *error,
    size_t error_len
) {
    if (app == NULL || app != g_active_app || app->runtime == NULL) {
        copy_error(error, error_len, "invalid or inactive mobile app handle");
        return NULANG_MOBILE_INVALID_ARGUMENT;
    }

    NulangValue value = nulang_run(app->runtime, app->module_handle);
    const char *runtime_error = nulang_last_error(app->runtime);
    if (runtime_error != NULL) {
        copy_error(error, error_len, runtime_error);
        return NULANG_MOBILE_RUN_ERROR;
    }

    if (out_value != NULL) {
        *out_value = value;
    }
    copy_error(error, error_len, "");
    return NULANG_MOBILE_OK;
}

void nulang_mobile_app_free(NulangMobileApp *app) {
    if (app == NULL) {
        return;
    }

    if (app == g_active_app) {
        g_active_app = NULL;
        g_callbacks = (NulangMobileCallbacks){0};
    }

    if (app->runtime != NULL) {
        nulang_runtime_free(app->runtime);
        app->runtime = NULL;
    }

    free(app);
}
