package main

import (
	"fmt"
	"os"
	"path/filepath"
	"syscall"
)

// The OS releases this lock even after an unclean exit. A second launcher cannot
// mutate the same native cache or start another Player for an occupied session.
func lockGoSession(output string) (*os.File, error) {
	path := filepath.Join(output, ".session.lock")
	fd, err := syscall.Open(path, syscall.O_CREAT|syscall.O_RDWR|syscall.O_CLOEXEC|syscall.O_NOFOLLOW, 0600)
	if err != nil {
		return nil, fmt.Errorf("open Go session lock: %w", err)
	}
	file := os.NewFile(uintptr(fd), path)
	if err := syscall.Flock(fd, syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		file.Close()
		return nil, fmt.Errorf("Go session output is already in use (%s): %w", output, err)
	}
	return file, nil
}
