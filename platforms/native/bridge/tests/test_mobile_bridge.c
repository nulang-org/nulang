#include "nulang_mobile_host.h"

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct NulangRuntime {
    const char *last_error;
};

struct NulangMobileActionRuntime {
    int ready;
    const char *last_error;
    char result[256];
};

typedef void (*RegisteredStringFn)(const char *);

static RegisteredStringFn registered_document = NULL;
static RegisteredStringFn registered_message = NULL;
static int runtime_new_calls = 0;
static int runtime_free_calls = 0;
static int load_calls = 0;
static int run_calls = 0;
static int action_new_calls = 0;
static int action_invoke_calls = 0;
static int action_free_calls = 0;

NulangRuntime *nulang_runtime_new_interpreter(void) {
    NulangRuntime *runtime = (NulangRuntime *)calloc(1, sizeof(*runtime));
    runtime_new_calls++;
    return runtime;
}

void nulang_runtime_free(NulangRuntime *runtime) {
    runtime_free_calls++;
    free(runtime);
}

int64_t nulang_load_nbc(
    NulangRuntime *runtime,
    const uint8_t *bytes,
    size_t len
) {
    load_calls++;
    if (len < 4 || memcmp(bytes, "NLBC", 4) != 0) {
        runtime->last_error = "invalid .nbc";
        return -1;
    }
    runtime->last_error = NULL;
    return 7;
}

NulangValue nulang_run(NulangRuntime *runtime, int64_t module_handle) {
    NulangValue result = {42};
    run_calls++;
    runtime->last_error = NULL;
    assert(module_handle == 7);

    if (registered_document != NULL) {
        registered_document(
            "{\"protocol\":\"nulang-ui/1\",\"platform\":\"ios\","
            "\"root\":{\"kind\":\"text\",\"value\":\"hello\"}}"
        );
    }
    if (registered_message != NULL) {
        registered_message(
            "{\"protocol\":\"nulang-ui-msg/1\",\"message\":{"
            "\"type\":\"signal.set\",\"name\":\"count\",\"value\":4}}"
        );
    }

    return result;
}

const char *nulang_last_error(NulangRuntime *runtime) {
    return runtime->last_error;
}

int64_t nulang_value_int(NulangValue value) {
    return (int64_t)value.raw;
}

int32_t nulang_register_native_function(
    const char *name,
    const void *ptr,
    const NulangCType *params,
    size_t param_count,
    NulangCType ret
) {
    assert(ptr != NULL);
    assert(params != NULL);
    assert(param_count == 1);
    assert(params[0] == NULANG_CTYPE_CSTR);
    assert(ret == NULANG_CTYPE_UNIT);

    if (strcmp(name, "nulang_ui_document") == 0) {
        registered_document = (RegisteredStringFn)ptr;
        return 0;
    }
    if (strcmp(name, "nulang_ui_message") == 0) {
        registered_message = (RegisteredStringFn)ptr;
        return 0;
    }
    return -1;
}

NulangMobileActionRuntime *nulang_mobile_action_runtime_new(
    const uint8_t *bytes,
    size_t len
) {
    NulangMobileActionRuntime *runtime =
        (NulangMobileActionRuntime *)calloc(1, sizeof(*runtime));
    assert(runtime != NULL);
    action_new_calls++;

    if (bytes == NULL || len < 4 || memcmp(bytes, "NLBC", 4) != 0) {
        runtime->ready = 0;
        runtime->last_error = "invalid mobile action artifact";
        return runtime;
    }
    if (len > 4 && bytes[4] == 0xEE) {
        runtime->ready = 0;
        runtime->last_error = "invalid mobile action metadata";
        return runtime;
    }

    runtime->ready = 1;
    runtime->last_error = NULL;
    return runtime;
}

bool nulang_mobile_action_runtime_is_ready(
    const NulangMobileActionRuntime *runtime
) {
    return runtime != NULL && runtime->ready != 0;
}

const char *nulang_mobile_action_runtime_invoke(
    NulangMobileActionRuntime *runtime,
    const char *request_json
) {
    action_invoke_calls++;
    if (runtime == NULL || !runtime->ready || request_json == NULL) {
        return NULL;
    }
    if (strstr(request_json, "\"handler\":\"save\"") == NULL) {
        runtime->last_error = "client action is not authorized";
        return NULL;
    }

    snprintf(
        runtime->result,
        sizeof(runtime->result),
        "%s",
        "{\"protocol\":\"nulang-action-result/1\","
        "\"correlation_id\":\"corr-bridge\",\"messages\":[]}"
    );
    runtime->last_error = NULL;
    return runtime->result;
}

const char *nulang_mobile_action_runtime_last_error(
    const NulangMobileActionRuntime *runtime
) {
    return runtime == NULL ? NULL : runtime->last_error;
}

void nulang_mobile_action_runtime_free(NulangMobileActionRuntime *runtime) {
    if (runtime != NULL) {
        action_free_calls++;
        free(runtime);
    }
}

typedef struct CallbackState {
    int documents;
    int messages;
    char last_document[512];
    char last_message[512];
} CallbackState;

static void on_document(const char *json, void *context) {
    CallbackState *state = (CallbackState *)context;
    state->documents++;
    snprintf(state->last_document, sizeof(state->last_document), "%s", json);
}

static void on_message(const char *json, void *context) {
    CallbackState *state = (CallbackState *)context;
    state->messages++;
    snprintf(state->last_message, sizeof(state->last_message), "%s", json);
}

int main(void) {
    static const uint8_t valid_nbc[] = {'N', 'L', 'B', 'C', 0, 0, 0, 1};
    char error[256] = {0};
    CallbackState callbacks = {0};
    NulangMobileCallbacks host = {
        .document = on_document,
        .message = on_message,
        .context = &callbacks,
    };

    NulangMobileApp *app = NULL;
    NulangMobileStatus status = nulang_mobile_app_new(
        valid_nbc,
        sizeof(valid_nbc),
        host,
        &app,
        error,
        sizeof(error)
    );
    assert(status == NULANG_MOBILE_OK);
    assert(app != NULL);
    assert(runtime_new_calls == 1);
    assert(load_calls == 1);
    assert(action_new_calls == 1);

    NulangMobileApp *second = NULL;
    status = nulang_mobile_app_new(
        valid_nbc,
        sizeof(valid_nbc),
        host,
        &second,
        error,
        sizeof(error)
    );
    assert(status == NULANG_MOBILE_ALREADY_ACTIVE);
    assert(second == NULL);
    assert(runtime_new_calls == 1);
    assert(action_new_calls == 1);

    NulangValue result = {0};
    status = nulang_mobile_app_run(app, &result, error, sizeof(error));
    assert(status == NULANG_MOBILE_OK);
    assert(run_calls == 1);
    assert(nulang_value_int(result) == 42);
    assert(callbacks.documents == 1);
    assert(callbacks.messages == 1);
    assert(strstr(callbacks.last_document, "nulang-ui/1") != NULL);
    assert(strstr(callbacks.last_message, "nulang-ui-msg/1") != NULL);

    const char *action_result = NULL;
    status = nulang_mobile_app_invoke_action(
        app,
        "{\"protocol\":\"nulang-action-invoke/1\","
        "\"handler\":\"save\",\"correlation_id\":\"corr-bridge\","
        "\"idempotency_key\":\"save:corr-bridge\",\"form\":{},\"signals\":{}}",
        &action_result,
        error,
        sizeof(error)
    );
    assert(status == NULANG_MOBILE_OK);
    assert(action_invoke_calls == 1);
    assert(action_result != NULL);
    assert(strstr(action_result, "nulang-action-result/1") != NULL);
    assert(strstr(action_result, "corr-bridge") != NULL);

    action_result = (const char *)0x1;
    status = nulang_mobile_app_invoke_action(
        app,
        "{\"protocol\":\"nulang-action-invoke/1\","
        "\"handler\":\"secret\",\"correlation_id\":\"corr-bridge\","
        "\"idempotency_key\":\"secret:corr-bridge\",\"form\":{},\"signals\":{}}",
        &action_result,
        error,
        sizeof(error)
    );
    assert(status == NULANG_MOBILE_ACTION_ERROR);
    assert(action_invoke_calls == 2);
    assert(action_result == NULL);
    assert(strstr(error, "not authorized") != NULL);

    nulang_mobile_app_free(app);
    assert(runtime_free_calls == 1);
    assert(action_free_calls == 1);

    app = NULL;
    status = nulang_mobile_app_new(
        valid_nbc,
        sizeof(valid_nbc),
        host,
        &app,
        error,
        sizeof(error)
    );
    assert(status == NULANG_MOBILE_OK);
    nulang_mobile_app_free(app);
    assert(runtime_free_calls == 2);
    assert(action_free_calls == 2);

    static const uint8_t invalid[] = {0, 1, 2, 3};
    app = NULL;
    status = nulang_mobile_app_new(
        invalid,
        sizeof(invalid),
        host,
        &app,
        error,
        sizeof(error)
    );
    assert(status == NULANG_MOBILE_ARTIFACT_ERROR);
    assert(app == NULL);
    assert(strstr(error, "invalid .nbc") != NULL);
    assert(runtime_free_calls == 3);
    assert(action_new_calls == 2);
    assert(action_free_calls == 2);

    static const uint8_t invalid_actions[] = {
        'N', 'L', 'B', 'C', 0xEE, 0, 0, 1
    };
    app = NULL;
    status = nulang_mobile_app_new(
        invalid_actions,
        sizeof(invalid_actions),
        host,
        &app,
        error,
        sizeof(error)
    );
    assert(status == NULANG_MOBILE_ARTIFACT_ERROR);
    assert(app == NULL);
    assert(strstr(error, "invalid mobile action metadata") != NULL);
    assert(runtime_free_calls == 4);
    assert(action_new_calls == 3);
    assert(action_free_calls == 3);

    puts("nulang mobile C bridge tests passed");
    return 0;
}
