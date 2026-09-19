package auth

import (
	"errors"
	"os"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
)

// errLockBusy marks "another mutation holds the lock right now".
var errLockBusy = errors.New("credential lock is busy")

// Lock is an advisory exclusive lock over <config>/.auth.lock held by login,
// auth import and logout / auth clear for the whole mutation window. It is
// released by unlock+close only; the lock file itself is never deleted to
// implement the mutual exclusion.
type Lock struct {
	f    *os.File
	path string
}

// AcquireLock takes the non-blocking exclusive lock. If another mutation is
// running it returns AUTH_BUSY immediately; it never kills or deletes
// anything owned by the other process.
func (s *Store) AcquireLock() (*Lock, error) {
	path := s.LockPath()
	if info, err := os.Lstat(path); err == nil {
		if !info.Mode().IsRegular() {
			return nil, apperr.Newf(apperr.IOError, apperr.StageCredentials,
				"锁文件 %s 不是普通文件，请手动检查", path)
		}
		if info.Mode().Perm()&0o077 != 0 {
			if err := os.Chmod(path, 0o600); err != nil {
				return nil, apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法收紧锁文件权限")
			}
		}
	} else if !os.IsNotExist(err) {
		return nil, apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法检查锁文件")
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法打开锁文件")
	}
	if err := lockHandle(f); err != nil {
		f.Close()
		if errors.Is(err, errLockBusy) {
			return nil, apperr.New(apperr.AuthBusy, apperr.StageCredentials,
				"另一个登录/导入/注销操作正在进行，请稍后再试")
		}
		return nil, apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法获取凭证修改锁")
	}
	return &Lock{f: f, path: path}, nil
}

// Release unlocks and closes. The lock file stays on disk.
func (l *Lock) Release() error {
	if l == nil || l.f == nil {
		return nil
	}
	err1 := unlockHandle(l.f)
	err2 := l.f.Close()
	l.f = nil
	if err1 != nil {
		return apperr.Wrap(err1, apperr.IOError, apperr.StageCredentials, "释放凭证修改锁失败")
	}
	if err2 != nil {
		return apperr.Wrap(err2, apperr.IOError, apperr.StageCredentials, "关闭锁文件失败")
	}
	return nil
}
