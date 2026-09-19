// Package login orchestrates the `sph login` flow: a dedicated visible
// Chromium (go-rod) with a throwaway profile, the user logging in
// personally, endpoint-scoped cookie capture, and an atomic commit only
// after the browser is closed. Failures never touch the existing
// credentials.
//
// Login detection is fully automatic: the cookie jar of the target endpoint
// is polled once a second and run through the loginWatch state machine
// (guest baseline settles → session-cookie names grow → fingerprint
// stabilises). A manual Enter in the terminal is accepted as a fallback when
// auto-detection misses.
package login

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"sph/internal/apperr"
	"sph/internal/auth"
)

// DefaultTimeout is the whole-login budget.
const DefaultTimeout = 5 * time.Minute

const (
	closeGrace       = 3 * time.Second
	pollInterval     = time.Second
	closedErrorLimit = 3
	// The single capture target: cookies applicable to this exact endpoint
	// are the only ones ever read or stored.
	targetEndpoint = "https://yuanbao.tencent.com/api/weixin/get_parse_result"
)

// Options carries every injectable dependency so tests never launch a real
// browser.
type Options struct {
	Timeout time.Duration

	// User I/O
	Stdin         io.Reader // the user's terminal
	Stderr        io.Writer // prompts and diagnostics, never secrets
	IsInteractive func() bool

	// Browser launches the dedicated login browser; defaults to go-rod.
	Browser func(ctx context.Context, workDir string, stderr io.Writer) (browserSession, error)

	Now func() time.Time
}

// Run performs the whole login. It returns apperr errors only; on any
// failure path the existing credentials are untouched.
func Run(parent context.Context, store *auth.Store, opts Options) error {
	if opts.Timeout <= 0 {
		opts.Timeout = DefaultTimeout
	}
	if opts.Now == nil {
		opts.Now = time.Now
	}
	if opts.IsInteractive == nil {
		return apperr.New(apperr.InternalError, apperr.StageLoginBrowser, "未配置终端检测")
	}
	if !opts.IsInteractive() {
		return apperr.New(apperr.InteractiveRequired, apperr.StageLoginBrowser,
			"login 需要交互式终端；请在本机终端中运行 sph login")
	}

	ctx, cancel := context.WithTimeout(parent, opts.Timeout)
	defer cancel()

	// One mutation at a time.
	lock, err := store.AcquireLock()
	if err != nil {
		return err
	}
	defer lock.Release()

	workDir, err := makeWorkDir(store.Dir)
	if err != nil {
		return err
	}
	defer cleanupWorkDir(workDir, opts.Stderr)

	launch := opts.Browser
	if launch == nil {
		launch = launchBrowser
	}
	fmt.Fprintf(opts.Stderr, "正在打开专用浏览器（元宝官网）…\n")
	session, err := launch(ctx, workDir, opts.Stderr)
	if err != nil {
		return err
	}
	committed := false
	defer func() {
		if !committed {
			session.Close()
		}
	}()

	fmt.Fprintf(opts.Stderr,
		"请在打开的浏览器窗口中登录 yuanbao.tencent.com（扫码或账号认证）。\n"+
			"登录后将自动继续并关闭浏览器；若长时间未自动继续，可回此终端按 Enter 手动继续；Ctrl+C 取消。\n")

	cookies, werr := waitForLogin(ctx, session, opts.Stdin)
	if werr != nil {
		return werr
	}

	creds, cerr := buildCredentials(session, cookies)
	if cerr != nil {
		return cerr
	}

	// Capture passed: close the browser FIRST, then commit.
	session.Close()
	committed = true
	time.Sleep(200 * time.Millisecond) // let the browser process fully exit before the profile is removed

	now := opts.Now().UTC()
	creds.Source = auth.SourceBrowserLogin
	creds.SavedAt = now
	creds.VerifiedAt = nil
	if err := store.Save(creds); err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageLoginCommit, "保存登录凭证失败")
	}
	fmt.Fprintf(opts.Stderr, "登录凭证已保存，浏览器已关闭。\n")
	return nil
}

// waitForLogin polls the target endpoint's cookie jar through the watch
// state machine until login is detected. A terminal Enter forces it early.
// Repeated cookie errors mean the browser window was closed by the user.
func waitForLogin(ctx context.Context, session browserSession, stdin io.Reader) ([]cookieEntry, error) {
	manual := make(chan struct{}, 1)
	go func() {
		_, err := readLine(stdin)
		if err == nil {
			select {
			case manual <- struct{}{}:
			default:
			}
		}
	}()

	watch := newLoginWatch()
	ticker := time.NewTicker(pollInterval)
	defer ticker.Stop()
	consecutiveErrors := 0
	for {
		select {
		case <-ctx.Done():
			return nil, classifyLoginCtx(ctx)
		case <-manual:
			cookies, err := session.Cookies(targetEndpoint)
			if err != nil {
				return nil, apperr.Wrap(err, apperr.LoginBrowserFailed, apperr.StageLoginBrowser,
					"浏览器已不可用")
			}
			return cookies, nil
		case <-ticker.C:
			cookies, err := session.Cookies(targetEndpoint)
			if err != nil {
				consecutiveErrors++
				if consecutiveErrors >= closedErrorLimit {
					return nil, apperr.New(apperr.Cancelled, apperr.StageLoginBrowser,
						"浏览器窗口已被关闭，登录取消")
				}
				continue
			}
			consecutiveErrors = 0
			if watch.poll(cookies) == "detected" {
				return cookies, nil
			}
		}
	}
}

// buildCredentials turns the captured cookie set into validated
// credentials: cookie header plus the browser's own User-Agent (the only
// non-sensitive header worth keeping).
func buildCredentials(session browserSession, cookies []cookieEntry) (auth.Credentials, error) {
	if len(cookies) == 0 {
		return auth.Credentials{}, apperr.New(apperr.AuthRequired, apperr.StageLoginCapture,
			"未采集到元宝 Cookie；请重新执行 sph login 并完成登录")
	}
	parts := make([]string, 0, len(cookies))
	for _, c := range cookies {
		parts = append(parts, c.Name+"="+c.Value)
	}
	headers := map[string]string{}
	if ua, err := session.UserAgent(); err == nil && ua != "" {
		headers["user-agent"] = ua
	}
	filtered, verr := auth.ValidateHeaderMap(headers)
	if verr != nil {
		return auth.Credentials{}, apperr.New(apperr.LoginCaptureFailed, apperr.StageLoginCapture,
			"采集到的请求头未通过安全校验")
	}
	creds := auth.Credentials{
		Version:        auth.CredentialsVersion,
		Source:         auth.SourceBrowserLogin,
		Cookie:         strings.Join(parts, "; "),
		YuanbaoHeaders: filtered,
	}
	if err := creds.Validate(); err != nil {
		return auth.Credentials{}, apperr.New(apperr.LoginCaptureFailed, apperr.StageLoginCapture,
			"采集到的凭证未通过格式校验")
	}
	return creds, nil
}

func classifyLoginCtx(ctx context.Context) *apperr.Error {
	if errors.Is(ctx.Err(), context.Canceled) {
		return apperr.Wrap(ctx.Err(), apperr.Cancelled, apperr.StageLoginBrowser, "登录已取消")
	}
	return apperr.Wrap(ctx.Err(), apperr.Timeout, apperr.StageLoginBrowser,
		"登录超时（包含等待人工登录与采集的时间）")
}

func makeWorkDir(configDir string) (string, error) {
	buf := make([]byte, 5)
	if _, err := rand.Read(buf); err != nil {
		return "", apperr.Wrap(err, apperr.IOError, apperr.StageLoginBrowser, "无法生成登录工作目录名")
	}
	dir := filepath.Join(configDir, ".login-"+hex.EncodeToString(buf))
	if err := os.Mkdir(dir, 0o700); err != nil {
		return "", apperr.Wrap(err, apperr.IOError, apperr.StageLoginBrowser, "无法创建登录工作目录")
	}
	return dir, nil
}

// cleanupWorkDir removes this login's private directory; failure is a
// warning with the exact residual path, never a silent success.
func cleanupWorkDir(dir string, stderr io.Writer) {
	if dir == "" {
		return
	}
	if err := os.RemoveAll(dir); err != nil {
		fmt.Fprintf(stderr, "警告：清理登录临时目录失败，请手动删除 %s\n", dir)
	}
}

// readLine reads a single line with a bounded buffer.
func readLine(r io.Reader) (string, error) {
	if r == nil {
		return "", io.EOF
	}
	br := bufio.NewReader(io.LimitReader(r, 8192))
	line, err := br.ReadString('\n')
	if err != nil && line == "" {
		return "", err
	}
	return strings.TrimRight(line, "\r\n"), nil
}
