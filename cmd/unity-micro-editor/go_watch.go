package main

import (
	"context"
	"encoding/binary"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

type sourceChange struct {
	path string
	at   time.Time
	err  error
}
type projectWatcher struct {
	file        *os.File
	events      chan sourceChange
	directories map[int]string
}

func newProjectWatcher(ctx context.Context, project string) (*projectWatcher, error) {
	fd, err := syscall.InotifyInit1(syscall.IN_CLOEXEC | syscall.IN_NONBLOCK)
	if err != nil {
		return nil, err
	}
	w := &projectWatcher{file: os.NewFile(uintptr(fd), "project-inotify"), events: make(chan sourceChange, 8192), directories: map[int]string{}}
	for _, name := range []string{"Assets", "Packages", "ProjectSettings"} {
		root := filepath.Join(project, name)
		if _, err := os.Stat(root); os.IsNotExist(err) {
			continue
		}
		if err := w.addTree(root); err != nil {
			w.Close()
			return nil, err
		}
	}
	go w.read(ctx)
	return w, nil
}
func (w *projectWatcher) Close() error { return w.file.Close() }
func (w *projectWatcher) addTree(root string) error {
	return filepath.WalkDir(root, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.Type()&os.ModeSymlink != 0 {
			return errors.New("source watch rejects symlinks: " + path)
		}
		if !entry.IsDir() {
			return nil
		}
		wd, err := syscall.InotifyAddWatch(int(w.file.Fd()), path, syscall.IN_CLOSE_WRITE|syscall.IN_MOVED_TO|syscall.IN_MOVED_FROM|syscall.IN_CREATE|syscall.IN_DELETE|syscall.IN_DELETE_SELF|syscall.IN_ONLYDIR)
		if err == nil {
			w.directories[wd] = path
		}
		return err
	})
}
func (w *projectWatcher) read(ctx context.Context) {
	stop := context.AfterFunc(ctx, func() { w.file.Close() })
	defer stop()
	buffer := make([]byte, 256<<10)
	for {
		n, err := w.file.Read(buffer)
		if err != nil {
			if ctx.Err() != nil || errors.Is(err, os.ErrClosed) {
				return
			}
			w.send(ctx, sourceChange{err: err})
			return
		}
		for offset := 0; offset+16 <= n; {
			wd := int(int32(binary.LittleEndian.Uint32(buffer[offset:])))
			mask := binary.LittleEndian.Uint32(buffer[offset+4:])
			size := int(binary.LittleEndian.Uint32(buffer[offset+12:]))
			if size > n-offset-16 {
				w.send(ctx, sourceChange{err: errors.New("truncated inotify event")})
				return
			}
			name := strings.TrimRight(string(buffer[offset+16:offset+16+size]), "\x00")
			offset += 16 + size
			if mask&syscall.IN_Q_OVERFLOW != 0 {
				w.send(ctx, sourceChange{err: errors.New("inotify queue overflow; restart to rescan source cache")})
				return
			}
			directory, ok := w.directories[wd]
			if !ok {
				continue
			}
			path := filepath.Join(directory, name)
			if mask&syscall.IN_ISDIR != 0 && mask&(syscall.IN_CREATE|syscall.IN_MOVED_TO) != 0 {
				if err := w.addTree(path); err != nil {
					w.send(ctx, sourceChange{err: err})
					return
				}
			}
			if mask&syscall.IN_IGNORED != 0 {
				delete(w.directories, wd)
				continue
			}
			if mask&syscall.IN_ISDIR != 0 || relevantSource(path) {
				w.send(ctx, sourceChange{path: path, at: time.Now()})
			}
		}
	}
}
func (w *projectWatcher) send(ctx context.Context, event sourceChange) {
	select {
	case w.events <- event:
	case <-ctx.Done():
	}
}
