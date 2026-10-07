package main

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"unitymicroeditor/internal/mcpserver"
)

func main() {
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	if err := mcpserver.Run(ctx, os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
