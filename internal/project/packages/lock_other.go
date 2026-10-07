//go:build !linux && !darwin && !freebsd && !openbsd && !netbsd && !dragonfly

package packages

import (
	"context"
	"errors"
)

func acquireResolveLock(context.Context, string) (func(), error) {
	return nil, errors.New("package resolver process locking is unsupported on this platform")
}
