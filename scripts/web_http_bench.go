// Optional native loopback-only client for the experimental Web HTTP benchmark.
// Go standard library only; never used by the Nulang runtime.
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"math"
	"net/http"
	"os"
	"sort"
	"strconv"
	"sync"
	"time"
)

type result struct {
	latency time.Duration
	status  int
	bytes   int64
	err     string
}

func main() {
	os.Exit(runBenchmark(os.Args[1:], os.Stdout, os.Stderr))
}

func runBenchmark(args []string, stdout, stderr io.Writer) int {
	flags := flag.NewFlagSet("web_http_bench", flag.ContinueOnError)
	flags.SetOutput(stderr)
	port := flags.Int("port", 0, "loopback port")
	path := flags.String("path", "/", "path")
	n := flags.Int("requests", 2000, "requests")
	c := flags.Int("concurrency", 4, "concurrency")
	size := flags.Int64("bytes", -1, "expected HTTP 200 body length")
	if err := flags.Parse(args); err != nil {
		if err == flag.ErrHelp {
			return 0
		}
		return 2
	}
	if *port < 1 || *port > 65535 || *n < 1 || *n > 100000 || *c < 1 || *c > 64 || *size < -1 || flags.NArg() != 0 || len(*path) == 0 || (*path)[0] != '/' || (len(*path) > 1 && (*path)[1] == '/') {
		fmt.Fprintln(stderr, "invalid arguments")
		return 2
	}
	tr := http.DefaultTransport.(*http.Transport).Clone()
	tr.Proxy = nil
	tr.DisableKeepAlives = true
	tr.ForceAttemptHTTP2 = false
	client := &http.Client{Transport: tr, Timeout: 3 * time.Second, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	defer tr.CloseIdleConnections()
	url := "http://127.0.0.1:" + strconv.Itoa(*port) + *path
	obs := make([]result, *n)
	var wg sync.WaitGroup
	started := time.Now()
	for worker := 0; worker < *c; worker++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			for i := w; i < *n; i += *c {
				begin := time.Now()
				resp, err := client.Get(url)
				r := result{}
				if err != nil {
					r.err = "request"
				} else {
					r.status = resp.StatusCode
					r.bytes, err = io.Copy(io.Discard, resp.Body)
					closeErr := resp.Body.Close()
					if err != nil || closeErr != nil {
						r.err = "body"
					}
					if r.err == "" && r.status == 200 && *size >= 0 && r.bytes != *size {
						r.err = "wrong_body_size"
					}
				}
				r.latency = time.Since(begin)
				obs[i] = r
			}
		}(worker)
	}
	wg.Wait()
	elapsed := time.Since(started).Seconds()
	statuses := map[string]int{}
	errors := map[string]int{}
	samples := make([]time.Duration, 0, *n)
	failures := 0
	var bytes int64
	for _, r := range obs {
		status := strconv.Itoa(r.status)
		if r.status == 0 {
			status = "transport_error"
		}
		statuses[status]++
		bytes += r.bytes
		if r.err != "" {
			errors[r.err]++
		}
		if r.status != 200 || r.err != "" {
			failures++
		} else {
			samples = append(samples, r.latency)
		}
	}
	sort.Slice(samples, func(i, j int) bool { return samples[i] < samples[j] })
	pct := func(p float64) float64 {
		if len(samples) == 0 {
			return 0
		}
		return float64(samples[int(math.Ceil(float64(len(samples))*p))-1]) / float64(time.Millisecond)
	}
	report := map[string]any{
		"schema": 1, "host": "127.0.0.1", "port": *port, "path": *path,
		"concurrency": *c, "expected_status": 200, "expected_body_bytes": *size,
		"requests": *n, "failures": failures, "statuses": statuses,
		"error_types": errors, "body_bytes_total": bytes, "wall_seconds": elapsed,
		"throughput_rps": float64(*n) / elapsed, "latency_ms_p50": pct(.5),
		"latency_ms_p95": pct(.95), "latency_ms_max": pct(1),
	}
	if err := json.NewEncoder(stdout).Encode(report); err != nil {
		fmt.Fprintln(stderr, err)
		return 2
	}
	if failures > 0 {
		return 2
	}
	return 0
}
