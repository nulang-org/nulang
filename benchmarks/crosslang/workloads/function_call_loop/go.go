package main

import "fmt"

func add(x, y int64) int64 {
	return x + y
}

func main() {
	var sum int64
	var i int64
	for i < 9000000 {
		sum = add(sum, i*3-i/7)
		if sum > 1000000000 {
			sum -= 1000000000
		}
		i++
	}
	fmt.Println(sum)
}
