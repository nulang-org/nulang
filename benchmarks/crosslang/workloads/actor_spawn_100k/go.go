package main

import "fmt"

func main() {
	const n = 100000
	release := make(chan struct{})

	for i := 0; i < n; i++ {
		go func() {
			<-release
		}()
	}

	fmt.Println(n)
}
