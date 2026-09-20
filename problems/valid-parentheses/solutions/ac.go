package main

import (
	"bufio"
	"fmt"
	"os"
	"strings"
)

func valid(s string) bool {
	pairs := map[byte]byte{')': '(', ']': '[', '}': '{'}
	stack := make([]byte, 0, len(s))
	for i := 0; i < len(s); i++ {
		ch := s[i]
		if open, closing := pairs[ch]; closing {
			if len(stack) == 0 || stack[len(stack)-1] != open {
				return false
			}
			stack = stack[:len(stack)-1]
		} else {
			stack = append(stack, ch)
		}
	}
	return len(stack) == 0
}

func main() {
	reader := bufio.NewReaderSize(os.Stdin, 1<<20)
	line, _ := reader.ReadString('\n')
	fmt.Println(valid(strings.TrimSpace(line)))
}
