package auth

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
)

func newTestStore(t *testing.T) *Store {
	t.Helper()
	dir := filepath.Join(t.TempDir(), "cfg")
	store, err := NewStore(dir)
	if err != nil {
		t.Fatalf("NewStore: %v", err)
	}
	return store
}

func sampleCreds() Credentials {
	now := time.Date(2026, 9, 19, 0, 0, 0, 0, time.UTC)
	return Credentials{
		Version:        CredentialsVersion,
		SavedAt:        now,
		Source:         SourceBrowserLogin,
		VerifiedAt:     &now,
		Cookie:         "session=abc; token=def",
		YuanbaoHeaders: map[string]string{"user-agent": "UA/1"},
	}
}

func TestSaveLoadRoundtrip(t *testing.T) {
	store := newTestStore(t)
	in := sampleCreds()
	if err := store.Save(in); err != nil {
		t.Fatalf("save: %v", err)
	}
	info, err := os.Stat(store.Path())
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o600 {
		t.Errorf("file perm %o, want 600", info.Mode().Perm())
	}
	if info.Mode().Perm()&0o077 != 0 {
		t.Errorf("file readable by others")
	}
	out, err := store.Load()
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	if out.Cookie != in.Cookie || out.Source != in.Source || out.Version != in.Version {
		t.Errorf("roundtrip mismatch: %+v", out)
	}
	if out.VerifiedAt == nil || !out.VerivedAtEqual(in.VerifiedAt) {
		t.Errorf("verified_at mismatch")
	}
	if out.YuanbaoHeaders["user-agent"] != "UA/1" {
		t.Errorf("headers mismatch")
	}
}

func TestLoadMissingIsAuthRequired(t *testing.T) {
	store := newTestStore(t)
	_, err := store.Load()
	if apperr.CodeOf(err) != apperr.AuthRequired {
		t.Errorf("code %v, want AUTH_REQUIRED", apperr.CodeOf(err))
	}
}

func TestLoadLegacyFormatCompat(t *testing.T) {
	store := newTestStore(t)
	// Old-format file: no version/source/verified_at.
	legacy := map[string]any{
		"cookie":          "a=b",
		"yuanbao_headers": map[string]string{},
	}
	raw, _ := json.Marshal(legacy)
	if err := os.WriteFile(store.Path(), raw, 0o600); err != nil {
		t.Fatal(err)
	}
	creds, err := store.Load()
	if err != nil {
		t.Fatalf("legacy load failed: %v", err)
	}
	if creds.Source != SourceManualImport || creds.VerifiedAt != nil {
		t.Errorf("legacy defaults wrong: %+v", creds)
	}
}

func TestLoadUnknownVersionRejected(t *testing.T) {
	store := newTestStore(t)
	raw := `{"version": 99, "cookie": "a=b"}`
	if err := os.WriteFile(store.Path(), []byte(raw), 0o600); err != nil {
		t.Fatal(err)
	}
	_, err := store.Load()
	if apperr.CodeOf(err) != apperr.InvalidCredentials {
		t.Errorf("code %v, want INVALID_CREDENTIALS", apperr.CodeOf(err))
	}
}

func TestLoadSymlinkRejected(t *testing.T) {
	store := newTestStore(t)
	real := filepath.Join(t.TempDir(), "real.json")
	os.WriteFile(real, []byte(`{"cookie":"a=b"}`), 0o600)
	if err := os.Symlink(real, store.Path()); err != nil {
		t.Skip("symlink not supported here")
	}
	_, err := store.Load()
	if apperr.CodeOf(err) != apperr.InvalidCredentials {
		t.Errorf("symlink must be refused, got %v", apperr.CodeOf(err))
	}
}

func TestLoadWidePermsRejected(t *testing.T) {
	store := newTestStore(t)
	if err := store.Save(sampleCreds()); err != nil {
		t.Fatal(err)
	}
	os.Chmod(store.Path(), 0o644)
	_, err := store.Load()
	if apperr.CodeOf(err) != apperr.InvalidCredentials {
		t.Errorf("wide perms must be refused with hint, got %v", apperr.CodeOf(err))
	}
}

func TestLoadOversizeRejected(t *testing.T) {
	store := newTestStore(t)
	big := `{"cookie":"` + strings.Repeat("a", MaxCredentialsBytes) + `"}`
	os.WriteFile(store.Path(), []byte(big), 0o600)
	_, err := store.Load()
	if apperr.CodeOf(err) != apperr.InvalidCredentials {
		t.Errorf("oversize must be refused, got %v", apperr.CodeOf(err))
	}
}

func TestClearIdempotentAndScoped(t *testing.T) {
	store := newTestStore(t)
	other := filepath.Join(store.Dir, "other-file.txt")
	os.WriteFile(other, []byte("keep me"), 0o600)
	if err := store.Clear(); err != nil {
		t.Fatalf("clear with no creds should succeed: %v", err)
	}
	if err := store.Save(sampleCreds()); err != nil {
		t.Fatal(err)
	}
	if err := store.Clear(); err != nil {
		t.Fatalf("clear: %v", err)
	}
	if store.Exists() {
		t.Errorf("credentials still present")
	}
	if _, err := os.Stat(other); err != nil {
		t.Errorf("clear deleted an unrelated file")
	}
}

func TestLockMutualExclusion(t *testing.T) {
	store := newTestStore(t)
	lock1, err := store.AcquireLock()
	if err != nil {
		t.Fatalf("first lock: %v", err)
	}
	if _, err := store.AcquireLock(); apperr.CodeOf(err) != apperr.AuthBusy {
		t.Fatalf("second concurrent lock must be AUTH_BUSY, got %v", err)
	}
	if err := lock1.Release(); err != nil {
		t.Fatalf("release: %v", err)
	}
	lock2, err := store.AcquireLock()
	if err != nil {
		t.Fatalf("re-acquire after release: %v", err)
	}
	lock2.Release()
	// lock file must still exist (unlock ≠ delete)
	if _, err := os.Stat(store.LockPath()); err != nil {
		t.Errorf("lock file should persist: %v", err)
	}
}

func TestLockReleasedOnProcessExitSemantics(t *testing.T) {
	// flock is per-fd; a second Store instance in the same process also uses
	// a new fd, so the same mutual exclusion must hold.
	store := newTestStore(t)
	lock1, _ := store.AcquireLock()
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		if _, err := store.AcquireLock(); apperr.CodeOf(err) != apperr.AuthBusy {
			t.Errorf("goroutine lock must be AUTH_BUSY, got %v", err)
		}
	}()
	wg.Wait()
	lock1.Release()
}

func TestParseCookieImport(t *testing.T) {
	ok := map[string]string{
		"session=abc; x=y":        "session=abc; x=y",
		"  session=abc  ":         "session=abc",
		"Cookie: session=abc":     "session=abc",
		"cookie: session=abc":     "session=abc",
		"\tcookie: session=abc\n": "session=abc",
	}
	for in, want := range ok {
		got, err := ParseCookieImport(in)
		if err != nil {
			t.Errorf("%q: unexpected error %v", in, err)
			continue
		}
		if got != want {
			t.Errorf("%q: %q, want %q", in, got, want)
		}
	}
	bad := []string{
		"",
		"   ",
		"cookie=abc\r\nx=1",
		"multi\nline=1",
		"curl 'https://yuanbao.tencent.com/' -H 'cookie: x=1'",
		"cookie:",
		"just-a-token-without-equals",
	}
	for _, in := range bad {
		if _, err := ParseCookieImport(in); err == nil {
			t.Errorf("%q: expected rejection", in)
		}
	}
}

func TestParseHeadersFile(t *testing.T) {
	good, err := ParseHeadersFile([]byte(`{"User-Agent":"UA/1","x-language":"zh-CN"}`))
	if err != nil {
		t.Fatalf("good file rejected: %v", err)
	}
	if good["user-agent"] != "UA/1" || good["x-language"] != "zh-CN" {
		t.Errorf("canonicalisation failed: %v", good)
	}
	if _, err := ParseHeadersFile(nil); err != nil {
		t.Errorf("empty file should be a no-op")
	}
	bad := []string{
		`{"x-not-a-header":"v"}`, // unknown key
		`{"cookie":"x=1"}`,       // forbidden key
		`{"authorization":"Bearer x"}`,
		`{"referer":"https://evil.test/"}`,
		`{"referer":"http://yuanbao.tencent.com/"}`,
		`{"user-agent":"bad\r\nvalue"}`,
		`[1,2]`,              // not an object
		`{"user-agent":123}`, // not a string value
	}
	for _, in := range bad {
		if _, err := ParseHeadersFile([]byte(in)); err == nil {
			t.Errorf("%s: expected rejection", in)
		}
	}
}

func TestValidateHeaderMapSizeCap(t *testing.T) {
	in := map[string]string{}
	for i := 0; i < 60; i++ {
		in["x-hy"+string(rune('a'+i%26))+string(rune('0'+i/26))] = ""
	}
	// pump one allowed key with a huge value
	in["user-agent"] = strings.Repeat("a", MaxHeadersBytes)
	if _, err := ValidateHeaderMap(in); err == nil {
		t.Errorf("expected size cap rejection")
	}
}

func TestSaveRejectsInvalidCreds(t *testing.T) {
	store := newTestStore(t)
	c := sampleCreds()
	c.Cookie = ""
	if err := store.Save(c); err == nil {
		t.Errorf("empty cookie must be rejected")
	}
	c = sampleCreds()
	c.Cookie = strings.Repeat("a", MaxCookieBytes+1)
	if err := store.Save(c); err == nil {
		t.Errorf("oversize cookie must be rejected")
	}
	c = sampleCreds()
	c.YuanbaoHeaders = map[string]string{"cookie": "x=1"}
	if err := store.Save(c); err == nil {
		t.Errorf("forbidden header must be rejected")
	}
}

func (c Credentials) VerivedAtEqual(other *time.Time) bool {
	if other == nil {
		return false
	}
	return c.VerifiedAt.Equal(*other)
}
