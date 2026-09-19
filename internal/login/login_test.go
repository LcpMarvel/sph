package login

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
	"github.com/LcpMarvel/sph-downloader/internal/auth"
)

// ---- fakes -----------------------------------------------------------------

// fakeBrowser scripts the cookie jar over time. Each poll consumes the next
// entry of cookieScript; after the script runs out the last entry repeats.
// cookiesErrAfter simulates the browser being closed: polls fail once the
// flag is armed.
type fakeBrowser struct {
	mu             sync.Mutex
	cookieScript   [][]cookieEntry
	poll           int
	ua             string
	cookiesBroken  bool
	closed         bool
	manualOverride chan struct{} // nil; placeholder for future tests
}

func (f *fakeBrowser) Cookies(targetURL string) ([]cookieEntry, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.cookiesBroken {
		return nil, errors.New("browser is closed")
	}
	if len(f.cookieScript) == 0 {
		return nil, nil
	}
	idx := f.poll
	if idx >= len(f.cookieScript) {
		idx = len(f.cookieScript) - 1
	}
	f.poll++
	return f.cookieScript[idx], nil
}

func (f *fakeBrowser) UserAgent() (string, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.ua == "" {
		return "", errors.New("no ua")
	}
	return f.ua, nil
}

func (f *fakeBrowser) Close() error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.closed = true
	return nil
}

func (f *fakeBrowser) wasClosed() bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.closed
}

// enterReader feeds lines to successive readLine calls.
type enterReader struct {
	lines []string
	pos   int
}

func (e *enterReader) Read(p []byte) (int, error) {
	if e.pos >= len(e.lines) {
		return 0, io.EOF
	}
	line := e.lines[e.pos] + "\n"
	e.pos++
	return copy(p, []byte(line)), nil
}

type blockingReader struct{}

func (blockingReader) Read(p []byte) (int, error) {
	time.Sleep(2 * time.Second)
	return 0, io.EOF
}

// ---- shared setup -----------------------------------------------------------

type env struct {
	store   *auth.Store
	session *fakeBrowser
	stderr  *bytes.Buffer
	stdin   *enterReader
	deps    Options
}

func guestJar() []cookieEntry {
	return []cookieEntry{{Name: "guest_a", Value: "1"}, {Name: "guest_b", Value: "2"}}
}

func loggedInJar() []cookieEntry {
	return []cookieEntry{
		{Name: "guest_a", Value: "1"},
		{Name: "guest_b", Value: "2"},
		{Name: "session", Value: "DO_NOT_LEAK_COOKIE_123"},
		{Name: "token", Value: "t"},
	}
}

// script: guest cookies for the baseline phase, then a logged-in jar.
func defaultScript() [][]cookieEntry {
	return [][]cookieEntry{
		guestJar(), guestJar(), guestJar(), guestJar(),
		loggedInJar(), loggedInJar(), loggedInJar(), loggedInJar(),
	}
}

func newEnv(t *testing.T) *env {
	t.Helper()
	dir := filepath.Join(t.TempDir(), "cfg")
	store, err := auth.NewStore(dir)
	if err != nil {
		t.Fatal(err)
	}
	session := &fakeBrowser{cookieScript: defaultScript(), ua: "UA-TEST/1"}
	stderr := &bytes.Buffer{}
	stdin := &enterReader{lines: nil}
	e := &env{store: store, session: session, stderr: stderr, stdin: stdin}
	e.deps = Options{
		Timeout:       time.Minute,
		Stdin:         stdin,
		Stderr:        stderr,
		IsInteractive: func() bool { return true },
		Browser: func(ctx context.Context, workDir string, w io.Writer) (browserSession, error) {
			return session, nil
		},
		Now: func() time.Time { return time.Date(2026, 9, 19, 12, 0, 0, 0, time.UTC) },
	}
	return e
}

func (e *env) run(t *testing.T) error {
	t.Helper()
	return Run(context.Background(), e.store, e.deps)
}

// jar wraps cookie sets into a one-shot script.
func jar(cookies ...cookieEntry) [][]cookieEntry {
	return [][]cookieEntry{cookies}
}

func fileHash(path string) string {
	raw, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	sum := sha256.Sum256(raw)
	return hex.EncodeToString(sum[:])
}

// ---- the tests ----------------------------------------------------------------

func TestLoginSavesCredentials(t *testing.T) {
	e := newEnv(t)
	if err := e.run(t); err != nil {
		t.Fatalf("login: %v", err)
	}
	creds, err := e.store.Load()
	if err != nil {
		t.Fatal(err)
	}
	if creds.Source != auth.SourceBrowserLogin {
		t.Errorf("source %s", creds.Source)
	}
	if !strings.Contains(creds.Cookie, "session=DO_NOT_LEAK_COOKIE_123") {
		t.Errorf("session cookie not captured: %q", creds.Cookie)
	}
	if creds.VerifiedAt != nil {
		t.Errorf("browser login saves verified_at = null")
	}
	if creds.YuanbaoHeaders["user-agent"] != "UA-TEST/1" {
		t.Errorf("browser user-agent not kept: %v", creds.YuanbaoHeaders)
	}
	if !e.session.wasClosed() {
		t.Errorf("browser must be closed")
	}
	if strings.Contains(e.stderr.String(), "DO_NOT_LEAK_COOKIE_123") {
		t.Errorf("stderr leaked cookie")
	}
	if !strings.Contains(e.stderr.String(), "已保存") {
		t.Errorf("success message missing: %s", e.stderr.String())
	}
}

func TestLoginNonInteractive(t *testing.T) {
	e := newEnv(t)
	e.deps.IsInteractive = func() bool { return false }
	err := e.run(t)
	if apperr.CodeOf(err) != apperr.InteractiveRequired {
		t.Errorf("want INTERACTIVE_REQUIRED, got %v", err)
	}
}

func TestLoginLaunchFailure(t *testing.T) {
	e := newEnv(t)
	e.deps.Browser = func(ctx context.Context, workDir string, w io.Writer) (browserSession, error) {
		return nil, apperr.New(apperr.LoginBrowserFailed, apperr.StageLoginBrowser, "无法启动登录浏览器")
	}
	err := e.run(t)
	if apperr.CodeOf(err) != apperr.LoginBrowserFailed {
		t.Errorf("want LOGIN_BROWSER_FAILED, got %v", err)
	}
	if e.store.Exists() {
		t.Errorf("nothing must be saved")
	}
}

func TestLoginLockBusy(t *testing.T) {
	e := newEnv(t)
	lock, err := e.store.AcquireLock()
	if err != nil {
		t.Fatal(err)
	}
	defer lock.Release()
	if err := e.run(t); apperr.CodeOf(err) != apperr.AuthBusy {
		t.Errorf("want AUTH_BUSY, got %v", err)
	}
}

func TestLoginCaptureFailureKeepsOldCredentials(t *testing.T) {
	e := newEnv(t)
	old := auth.Credentials{
		Version: 1, Source: auth.SourceManualImport,
		SavedAt: time.Now(), Cookie: "old=1",
	}
	if err := e.store.Save(old); err != nil {
		t.Fatal(err)
	}
	before := fileHash(e.store.Path())
	// jar stays empty: manual Enter captures nothing
	e.session.cookieScript = jar()
	e.stdin.lines = []string{""} // manual Enter to force capture of nothing
	err := e.run(t)
	if err == nil {
		t.Fatal("expected failure")
	}
	if apperr.CodeOf(err) != apperr.AuthRequired {
		t.Errorf("want AUTH_REQUIRED (no cookie), got %v", err)
	}
	if after := fileHash(e.store.Path()); before != after {
		t.Errorf("failed login overwrote old credentials")
	}
	if !e.session.wasClosed() {
		t.Errorf("browser must still be closed on failure")
	}
}

func TestLoginManualEnterFallback(t *testing.T) {
	e := newEnv(t)
	// constant jar: no growth beyond baseline, auto detection can't fire;
	// manual Enter captures whatever the jar holds at that moment
	e.session.cookieScript = [][]cookieEntry{loggedInJar()}
	e.stdin.lines = []string{""}
	if err := e.run(t); err != nil {
		t.Fatalf("manual fallback login: %v", err)
	}
	creds, _ := e.store.Load()
	if !strings.Contains(creds.Cookie, "session=DO_NOT_LEAK_COOKIE_123") {
		t.Errorf("manual fallback did not capture the logged-in jar: %q", creds.Cookie)
	}
}

func TestLoginTimeoutWaitingForLogin(t *testing.T) {
	e := newEnv(t)
	// jar stuck on guest cookies forever
	e.session.cookieScript = jar(cookieEntry{Name: "guest_a", Value: "1"})
	e.deps.Timeout = 200 * time.Millisecond
	e.deps.Stdin = blockingReader{}
	err := e.run(t)
	if apperr.CodeOf(err) != apperr.Timeout {
		t.Errorf("want TIMEOUT, got %v", err)
	}
	if !e.session.wasClosed() {
		t.Errorf("timeout must close the browser")
	}
}

func TestLoginCancelDuringWait(t *testing.T) {
	e := newEnv(t)
	e.session.cookieScript = jar(cookieEntry{Name: "guest_a", Value: "1"})
	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		time.Sleep(150 * time.Millisecond)
		cancel()
	}()
	e.deps.Stdin = blockingReader{}
	err := Run(ctx, e.store, e.deps)
	if apperr.CodeOf(err) != apperr.Cancelled {
		t.Errorf("want CANCELLED, got %v", err)
	}
}

func TestLoginBrowserClosedByUser(t *testing.T) {
	e := newEnv(t)
	e.session.cookieScript = jar(cookieEntry{Name: "guest_a", Value: "1"})
	e.deps.Browser = func(ctx context.Context, workDir string, w io.Writer) (browserSession, error) {
		// break the jar shortly after launch
		go func() {
			time.Sleep(1200 * time.Millisecond)
			e.session.mu.Lock()
			e.session.cookiesBroken = true
			e.session.mu.Unlock()
		}()
		return e.session, nil
	}
	e.deps.Stdin = blockingReader{}
	err := e.run(t)
	if apperr.CodeOf(err) != apperr.Cancelled {
		t.Errorf("want CANCELLED on browser close, got %v", err)
	}
	if !strings.Contains(e.stderr.String(), "浏览器窗口已被关闭") &&
		!strings.Contains(err.Error(), "浏览器窗口已被关闭") {
		t.Errorf("message should attribute cancellation to the closed window")
	}
}

func TestLoginNoSecretsInOutput(t *testing.T) {
	e := newEnv(t)
	e.session.cookieScript = [][]cookieEntry{
		{}, {}, {},
		{{Name: "session", Value: "DO_NOT_LEAK_COOKIE_123"}},
		{{Name: "session", Value: "DO_NOT_LEAK_COOKIE_123"}},
		{{Name: "session", Value: "DO_NOT_LEAK_COOKIE_123"}},
	}
	if err := e.run(t); err != nil {
		t.Fatalf("login: %v", err)
	}
	if strings.Contains(e.stderr.String(), "DO_NOT_LEAK_COOKIE_123") {
		t.Errorf("stderr leaked cookie value")
	}
}

func TestRedirectStdoutBestEffort(t *testing.T) {
	restore, err := redirectStdoutToStderr()
	if err != nil {
		t.Skipf("redirect unavailable: %v", err)
	}
	fmtOriginal := os.Stdout
	restore()
	if os.Stdout != fmtOriginal {
		t.Errorf("stdout must be restored")
	}
}
