package main

import "fmt"

func main() {
	const n = 250000
	mailbox := make(chan int, 1)
	done := make(chan int, 1)

	go func() {
		turns := 0
		mailbox <- n
		for {
			remaining := <-mailbox
			turns++
			if remaining <= 1 {
				done <- turns
				return
			}
			mailbox <- remaining - 1
		}
	}()

	fmt.Println(<-done)
}
