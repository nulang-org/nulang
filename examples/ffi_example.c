/* Minimal C example of embedding Nulang through the stable C API. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../include/nulang.h"

static void print_error(NulangRuntime *rt, const char *ctx) {
    const char *err = nulang_last_error(rt);
    if (err && strlen(err) > 0) {
        fprintf(stderr, "%s: %s\n", ctx, err);
    }
}

int main(void) {
    NulangRuntime *rt = nulang_runtime_new();
    if (!rt) {
        fprintf(stderr, "failed to create runtime\n");
        return 1;
    }

    const char *source =
        "extern \"libm.so.6\" {\n"
        "  fn sqrt(x: Float) -> Float\n"
        "}\n"
        "fn greet(name: String) -> String {\n"
        "  perform String.concat(\"hello \", name)\n"
        "}\n"
        "sqrt(9.0)\n";

    int64_t handle = nulang_compile(rt, source);
    if (handle < 0) {
        print_error(rt, "compile error");
        nulang_runtime_free(rt);
        return 1;
    }

    NulangValue result = nulang_run(rt, handle);
    print_error(rt, "run error");

    double f = nulang_value_float(result);
    printf("sqrt(9.0) = %f\n", f);

    const char *repr = nulang_value_to_string(rt, result);
    printf("string repr: %s\n", repr ? repr : "(null)");
    nulang_free_string(rt, repr);

    /* The owned-value API avoids leaking raw NaN-tagged values to the host. */
    NulangOwnedValueHandle name = nulang_owned_value_string_new(rt, "world");
    if (!name) {
        print_error(rt, "owned string creation");
        nulang_runtime_free(rt);
        return 1;
    }

    NulangOwnedValueHandle greeting =
        nulang_call_function_owned(rt, handle, "greet", &name, 1);
    if (!greeting) {
        print_error(rt, "managed function call");
        nulang_owned_value_release(rt, name);
        nulang_runtime_free(rt);
        return 1;
    }

    const char *greeting_str = nulang_owned_value_to_string(rt, greeting);
    printf("greet(\"world\") = %s\n", greeting_str ? greeting_str : "(null)");
    nulang_free_string(rt, greeting_str);
    nulang_owned_value_release(rt, greeting);
    nulang_owned_value_release(rt, name);

    nulang_runtime_free(rt);
    return 0;
}
