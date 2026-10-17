package main

import "fmt"

func main() {
	var sum int64
	var i int64
	for i < 9000000 {
		sum = sum + i*3 - i/7
		i++
	}
	fmt.Println(sum)
}
