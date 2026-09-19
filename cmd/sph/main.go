// Command sph downloads WeChat Channels (视频号) videos the user already has
// access to, using locally stored credentials. See README.md and .
package main

import (
	"context"
	"os"
	"os/signal"
	"syscall"

	"github.com/LcpMarvel/sph-downloader/internal/cli"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	code := cli.Run(ctx, os.Args[1:], os.Stdin, os.Stdout, os.Stderr, cli.DefaultDeps())
	os.Exit(code)
}
