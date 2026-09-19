package login

import (
	"context"
	"errors"
	"fmt"
	"io"
	"path/filepath"
	"syscall"
	"time"

	"github.com/go-rod/rod"
	"github.com/go-rod/rod/lib/launcher"
	"github.com/go-rod/rod/lib/proto"

	"sph/internal/apperr"
)

// browserSession abstracts the dedicated login browser so tests can run
// without ever launching Chromium.
type browserSession interface {
	// Cookies returns the cookies applicable to the target URL (HttpOnly
	// included), like the browser would send them to that endpoint.
	Cookies(targetURL string) ([]cookieEntry, error)
	// UserAgent returns the browser's own navigator.userAgent (non-sensitive).
	UserAgent() (string, error)
	Close() error
}

const homePage = "https://yuanbao.tencent.com/"

// rodSession is the production browserSession on top of go-rod: a dedicated
// visible Chromium with a private profile under the login work directory.
type rodSession struct {
	launcher *launcher.Launcher
	browser  *rod.Browser
	page     *rod.Page
}

// launchBrowser downloads the managed Chromium on first use (one time, into
// the user-level rod cache shared with other rod projects), then launches a
// dedicated visible instance with a throwaway profile.
func launchBrowser(ctx context.Context, workDir string, stderr io.Writer) (browserSession, error) {
	l := launcher.New().
		Headless(false).
		UserDataDir(filepath.Join(workDir, "browser-profile")).
		Set("window-size", "1280,860").
		// browser process output is diagnostics; keep it off our stdout
		Logger(stderr).
		Context(ctx)

	if _, has := launcher.LookPath(); !has {
		fmt.Fprintf(stderr, "首次登录：正在下载专用浏览器（约 200MB，仅此一次）…\n")
	}

	// rod's downloader logs progress to fd 1; stdout is reserved for final
	// results, so point fd 1 at stderr for the duration of the launch.
	restore, _ := redirectStdoutToStderr()
	controlURL, err := l.Launch()
	if restore != nil {
		restore()
	}
	if err != nil {
		return nil, apperr.Wrap(err, apperr.LoginBrowserFailed, apperr.StageLoginBrowser,
			"无法启动登录浏览器；首次使用需要联网下载浏览器，请检查网络后重试")
	}

	b := rod.New().Context(ctx).ControlURL(controlURL)
	if err := b.Connect(); err != nil {
		l.Kill()
		return nil, apperr.Wrap(err, apperr.LoginBrowserFailed, apperr.StageLoginBrowser,
			"无法连接登录浏览器")
	}
	page, err := b.Page(proto.TargetCreateTarget{URL: homePage})
	if err != nil {
		b.Close()
		l.Kill()
		return nil, apperr.Wrap(err, apperr.LoginBrowserFailed, apperr.StageLoginBrowser,
			"无法打开元宝官网页面")
	}
	// Bounded wait for the page; login detection only needs the cookie jar,
	// so a slow page is not fatal.
	_ = page.Timeout(30 * time.Second).WaitLoad()

	return &rodSession{launcher: l, browser: b, page: page}, nil
}

func (s *rodSession) Cookies(targetURL string) ([]cookieEntry, error) {
	cookies, err := s.page.Cookies([]string{targetURL})
	if err != nil {
		return nil, err
	}
	out := make([]cookieEntry, 0, len(cookies))
	for _, c := range cookies {
		out = append(out, cookieEntry{Name: c.Name, Value: c.Value})
	}
	return out, nil
}

func (s *rodSession) UserAgent() (string, error) {
	res, err := s.page.Eval(`() => navigator.userAgent`)
	if err != nil {
		return "", err
	}
	ua := res.Value.Str()
	if ua == "" {
		return "", errors.New("empty user agent")
	}
	return ua, nil
}

func (s *rodSession) Close() error {
	err := s.browser.Close() // Browser.close CDP command terminates the instance
	go s.launcher.Kill()     // belt and suspenders for our private instance
	return err
}

// redirectStdoutToStderr points fd 1 at fd 2 and returns a restore func.
// Best effort: when unavailable the launch simply proceeds unmuted.
func redirectStdoutToStderr() (func(), error) {
	saved, err := syscall.Dup(syscall.Stdout)
	if err != nil {
		return nil, err
	}
	if err := syscall.Dup2(syscall.Stderr, syscall.Stdout); err != nil {
		syscall.Close(saved)
		return nil, err
	}
	return func() {
		syscall.Dup2(saved, syscall.Stdout)
		syscall.Close(saved)
	}, nil
}
