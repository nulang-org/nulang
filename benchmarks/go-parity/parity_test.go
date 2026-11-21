package goparity

import "testing"

var intSink int64
var floatSink float64

func intLoop() int64 {
	var sum int64
	for i := int64(0); i < 100000; i++ {
		sum = sum + i*3 - i/7
	}
	return sum
}

func floatLoop() float64 {
	var x float64
	for i := 0; i < 100000; i++ {
		x = x*1.000001 + 0.25
	}
	return x
}

//go:noinline
func add(x, y int64) int64 {
	return x + y
}

func directCallLoop() int64 {
	var sum int64
	for i := int64(0); i < 100000; i++ {
		sum = add(sum, i)
	}
	return sum
}

//go:noinline
func fib(n int64) int64 {
	if n < 2 {
		return n
	}
	return fib(n-1) + fib(n-2)
}

func BenchmarkIntLoop(b *testing.B) {
	var out int64
	for i := 0; i < b.N; i++ {
		out = intLoop()
	}
	intSink = out
}

func BenchmarkFloatLoop(b *testing.B) {
	var out float64
	for i := 0; i < b.N; i++ {
		out = floatLoop()
	}
	floatSink = out
}

func BenchmarkDirectCallLoop(b *testing.B) {
	var out int64
	for i := 0; i < b.N; i++ {
		out = directCallLoop()
	}
	intSink = out
}

func BenchmarkFib25(b *testing.B) {
	var out int64
	for i := 0; i < b.N; i++ {
		out = fib(25)
	}
	intSink = out
}
