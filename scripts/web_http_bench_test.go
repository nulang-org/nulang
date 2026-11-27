package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strconv"
	"sync/atomic"
	"testing"
)

func TestNativeBenchmarkCLI(t *testing.T) {
	// Exercise the CLI entry point against an independent loopback fixture,
	// without invoking a nested toolchain or relying on network dependencies.
	var keepalive atomic.Bool
	var redirected atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !r.Close {
			keepalive.Store(true)
		}
		switch r.URL.Path {
		case "/ok":
			fmt.Fprint(w, "okay")
		case "/short":
			fmt.Fprint(w, "no")
		case "/overload":
			w.WriteHeader(http.StatusServiceUnavailable)
		case "/redirect":
			http.Redirect(w, r, "/followed", http.StatusFound)
		case "/followed":
			redirected.Add(1)
			fmt.Fprint(w, "okay")
		default:
			w.WriteHeader(http.StatusNotFound)
		}
	}))
	defer server.Close()
	parsed, err := url.Parse(server.URL)
	if err != nil {
		t.Fatal(err)
	}

	type report struct {
		Requests int            `json:"requests"`
		Failures int            `json:"failures"`
		Statuses map[string]int `json:"statuses"`
		Errors   map[string]int `json:"error_types"`
		Bytes    int64          `json:"body_bytes_total"`
		P95      float64        `json:"latency_ms_p95"`
	}
	for _, tc := range []struct {
		path      string
		size      int
		status    int
		failures  int
		errorType string
	}{
		{"/ok", 4, 200, 0, ""},
		{"/short", 4, 200, 16, "wrong_body_size"},
		{"/overload", 4, 503, 16, ""},
		{"/redirect", 4, 302, 16, ""},
	} {
		t.Run(tc.path, func(t *testing.T) {
			var out, stderr bytes.Buffer
			actualExit := runBenchmark([]string{"-port", parsed.Port(), "-path", tc.path, "-requests", "16", "-concurrency", "4", "-bytes", strconv.Itoa(tc.size)}, &out, &stderr)
			expectedExit := 0
			if tc.failures != 0 {
				expectedExit = 2
			}
			if actualExit != expectedExit {
				t.Fatalf("exit %d, want %d: %s %s", actualExit, expectedExit, out.String(), stderr.String())
			}
			var got report
			if err := json.Unmarshal(out.Bytes(), &got); err != nil {
				t.Fatal(err)
			}
			if got.Requests != 16 || got.Failures != tc.failures || got.Statuses[strconv.Itoa(tc.status)] != 16 {
				t.Fatalf("unexpected response accounting: %+v", got)
			}
			if tc.errorType != "" && got.Errors[tc.errorType] != 16 {
				t.Fatalf("unexpected errors: %+v", got.Errors)
			}
			if tc.failures == 0 && (got.Bytes != 64 || got.P95 <= 0) {
				t.Fatalf("unexpected successful report: %+v", got)
			}
			if tc.failures != 0 && got.P95 != 0 {
				t.Fatalf("failed requests must not contribute to success latency: %+v", got)
			}
		})
	}
	if keepalive.Load() {
		t.Fatal("benchmark must open a fresh connection per GET")
	}
	if redirected.Load() != 0 {
		t.Fatal("benchmark must not follow redirects")
	}
	for _, args := range [][]string{
		{"-port", "0"},
		{"-port", parsed.Port(), "-concurrency", "65"},
		{"-port", parsed.Port(), "-requests", "0"},
		{"-port", parsed.Port(), "-path", "//external"},
		{"-port", parsed.Port(), "-bytes", "-2"},
		{"-port", parsed.Port(), "extra"},
	} {
		if code := runBenchmark(args, &bytes.Buffer{}, &bytes.Buffer{}); code != 2 {
			t.Fatalf("accepted invalid arguments %v", args)
		}
	}
	server.Close()
	var out, stderr bytes.Buffer
	if code := runBenchmark([]string{"-port", parsed.Port(), "-requests", "4", "-concurrency", "2"}, &out, &stderr); code != 2 {
		t.Fatalf("closed listener must fail: %s %s", out.String(), stderr.String())
	}
	var failed report
	if err := json.Unmarshal(out.Bytes(), &failed); err != nil {
		t.Fatal(err)
	}
	if failed.Failures != 4 || failed.Statuses["transport_error"] != 4 || failed.Errors["request"] != 4 {
		t.Fatalf("unexpected transport failure accounting: %+v", failed)
	}
}
