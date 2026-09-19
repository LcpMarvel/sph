package cli

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"sph/internal/auth"
	"sph/internal/download"
	"sph/internal/netpolicy"
	"sph/internal/upstream"
	"sph/internal/verify"
)

// : three fake servers (yuanbao, finder preview, media) carrying
// sentinel secrets; no sentinel may appear anywhere in stdout/stderr of
// inspect or download, and credentials must only ever reach yuanbao.
const (
	sentinelCookie     = "DO_NOT_LEAK_COOKIE_123"
	sentinelToken      = "DO_NOT_LEAK_TOKEN_456"
	sentinelMediaQuery = "DO_NOT_LEAK_MEDIA_QUERY_789"
)

// mediaBody returns the bytes the fake media server serves. It prefers the
// self-generated tiny.mp4 fixture (real video+audio streams → ffprobe can
// fully verify); without the fixture it falls back to a synthetic container
// and the assertions relax to container-level.
func mediaBody() ([]byte, bool) {
	if raw, err := os.ReadFile(filepath.Join("..", "..", "testdata", "tiny.mp4")); err == nil && len(raw) > 0 {
		return raw, true
	}
	box := func(typ string, payload []byte) []byte {
		out := make([]byte, 8+len(payload))
		size := uint32(len(payload) + 8)
		out[0] = byte(size >> 24)
		out[1] = byte(size >> 16)
		out[2] = byte(size >> 8)
		out[3] = byte(size)
		copy(out[4:8], typ)
		copy(out[8:], payload)
		return out
	}
	data := box("ftyp", []byte("isom"))
	data = append(data, box("moov", make([]byte, 32))...)
	return append(data, box("mdat", make([]byte, 256))...), false
}

// leakEnv wires cli.Run at three local servers via an unrestricted client
// (the public-address dialer is unit-tested separately in netpolicy).
func leakEnv(t *testing.T) (Deps, *httptest.Server, *httptest.Server, *httptest.Server, string) {
	t.Helper()

	var finderSrv, mediaSrv *httptest.Server

	yuanbaoSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Cookie") != sentinelCookie {
			w.WriteHeader(401)
			return
		}
		// production-form playable_url: the client only parses it (token/eid);
		// the actual finder request is rewritten onto the local server by
		// rewriteTransport below.
		playable := "https://channels.weixin.qq.com/finder-preview/pages/feed?token=" + sentinelToken + "&eid=EID1"
		json.NewEncoder(w).Encode(map[string]any{
			"code": 0,
			"data": map[string]any{"playable_url": playable},
		})
	}))
	finderSrv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// production-form media URL (real CDN host, no port); mediaTransport
		// rewrites it onto the local media server while preserving the query.
		mediaURL := "https://cdn.example.com/v.mp4?sig=" + sentinelMediaQuery
		json.NewEncoder(w).Encode(map[string]any{
			"errCode": 0,
			"data": map[string]any{
				"feedInfo": map[string]any{
					"h264VideoInfo": map[string]any{"videoUrl": mediaURL},
					"description":   "泄露测试 标题",
					"picInfo":       []any{},
				},
				"authorInfo": map[string]any{"nickname": "作者"},
			},
		})
	}))
	mediaSrv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Cookie") != "" {
			w.WriteHeader(500)
			return
		}
		if !strings.HasPrefix(r.URL.RawQuery, "sig="+sentinelMediaQuery) {
			w.WriteHeader(404)
			return
		}
		w.Header().Set("Content-Type", "video/mp4")
		body, _ := mediaBody()
		w.Write(body)
	}))

	cfgDir := filepath.Join(t.TempDir(), "cfg")
	deps := Deps{
		ConfigDir:   func() string { return cfgDir },
		Interactive: func() bool { return false },
		NewUpstreamClient: func() *upstream.Client {
			c := upstream.NewClient(&http.Client{Transport: rewriteTransport{
				yuanbao: yuanbaoSrv.URL,
				finder:  finderSrv.URL,
			}})
			return c
		},
		NewMediaClient: func() download.HTTPClient {
			return &http.Client{Transport: mediaTransport{mediaSrv.URL}}
		},
		Now: time.Now,
	}
	return deps, yuanbaoSrv, finderSrv, mediaSrv, cfgDir
}

// rewriteTransport maps the production hosts onto the local test servers
// WITHOUT touching the request bodies or credentials.
type rewriteTransport struct {
	yuanbao, finder string
}

func (rt rewriteTransport) RoundTrip(r *http.Request) (*http.Response, error) {
	target := r.URL.String()
	switch {
	case strings.HasPrefix(target, "https://yuanbao.tencent.com/"):
		target = rt.yuanbao + target[len("https://yuanbao.tencent.com"):]
	case strings.HasPrefix(target, "https://channels.weixin.qq.com/"):
		target = rt.finder + target[len("https://channels.weixin.qq.com"):]
	}
	req, err := http.NewRequest(r.Method, target, r.Body)
	if err != nil {
		return nil, err
	}
	for k, vs := range r.Header {
		req.Header[k] = vs
	}
	return http.DefaultTransport.RoundTrip(req.WithContext(r.Context()))
}

type mediaTransport struct{ media string }

func (mt mediaTransport) RoundTrip(r *http.Request) (*http.Response, error) {
	target := mt.media + r.URL.Path + "?" + r.URL.RawQuery
	req, err := http.NewRequest(r.Method, target, nil)
	if err != nil {
		return nil, err
	}
	for k, vs := range r.Header {
		req.Header[k] = vs
	}
	return http.DefaultTransport.RoundTrip(req.WithContext(r.Context()))
}

func installCreds(t *testing.T, cfgDir string) {
	t.Helper()
	store, err := auth.NewStore(cfgDir)
	if err != nil {
		t.Fatal(err)
	}
	lock, err := store.AcquireLock()
	if err != nil {
		t.Fatal(err)
	}
	defer lock.Release()
	if err := store.Save(auth.Credentials{
		Version: 1, Source: auth.SourceManualImport,
		SavedAt: time.Now(), Cookie: sentinelCookie,
		YuanbaoHeaders: map[string]string{"x-id": "DO_NOT_LEAK_HEADER_321"},
	}); err != nil {
		t.Fatal(err)
	}
}

func assertNoSentinels(t *testing.T, outputs ...string) {
	t.Helper()
	sentinels := []string{
		sentinelCookie, sentinelToken, sentinelMediaQuery,
		"DO_NOT_LEAK_HEADER_321",
		"finder-preview/pages/feed?token",
	}
	for _, out := range outputs {
		for _, s := range sentinels {
			if strings.Contains(out, s) {
				t.Errorf("OUTPUT LEAK: sentinel %q found in:\n%s", s, out)
			}
		}
	}
}

func TestInspectE2EIsolationAndNoLeaks(t *testing.T) {
	deps, yb, fd, md, cfgDir := leakEnv(t)
	defer yb.Close()
	defer fd.Close()
	defer md.Close()
	installCreds(t, cfgDir)

	var out, errOut bytes.Buffer
	code := Run(context.Background(),
		[]string{"inspect", "https://weixin.qq.com/sph/LeakTest1", "--json"},
		strings.NewReader(""), &out, &errOut, deps)
	if code != 0 {
		t.Fatalf("inspect exit %d, stderr: %s", code, errOut.String())
	}
	assertNoSentinels(t, out.String(), errOut.String())

	var env struct {
		OK   bool `json:"ok"`
		Data struct {
			LocalID string `json:"local_id"`
			Title   string `json:"title"`
		} `json:"data"`
	}
	if err := json.Unmarshal(out.Bytes(), &env); err != nil {
		t.Fatalf("stdout not one JSON object: %v (%q)", err, out.String())
	}
	if !env.OK || env.Data.Title != "泄露测试 标题" || len(env.Data.LocalID) != 12 {
		t.Errorf("inspect payload wrong: %s", out.String())
	}
}

func TestDownloadE2EIsolationAndNoLeaks(t *testing.T) {
	deps, yb, fd, md, cfgDir := leakEnv(t)
	defer yb.Close()
	defer fd.Close()
	defer md.Close()
	installCreds(t, cfgDir)

	// ffprobe on PATH could interfere; force container-only verification by
	// hiding any real ffprobe through an injected verifier is not plumbed at
	// the CLI level, so run with the real one if present — both are valid.
	if !verify.HasFFProbe() {
		t.Log("ffprobe absent: download will verify container-only")
	}

	work := t.TempDir()
	var out, errOut bytes.Buffer
	code := Run(context.Background(),
		[]string{"download", "https://weixin.qq.com/sph/LeakTest2", "-o", filepath.Join(work, "out.mp4"), "--json"},
		strings.NewReader(""), &out, &errOut, deps)
	if code != 0 {
		t.Fatalf("download exit %d, stderr: %s", code, errOut.String())
	}
	assertNoSentinels(t, out.String(), errOut.String())

	var env struct {
		OK   bool `json:"ok"`
		Data struct {
			Path         string `json:"path"`
			Bytes        int64  `json:"bytes"`
			SHA256       string `json:"sha256"`
			Verification string `json:"verification"`
		} `json:"data"`
	}
	if err := json.Unmarshal(out.Bytes(), &env); err != nil {
		t.Fatalf("stdout not one JSON object: %v", err)
	}
	body, realVideo := mediaBody()
	if !env.OK || env.Data.Bytes != int64(len(body)) || env.Data.SHA256 == "" {
		t.Errorf("download payload wrong: %s", out.String())
	}
	if verify.HasFFProbe() && realVideo {
		if env.Data.Verification != "ffprobe" {
			t.Errorf("expected full ffprobe verification, got %q", env.Data.Verification)
		}
	} else if env.Data.Verification != "container" {
		t.Errorf("verification method wrong: %q", env.Data.Verification)
	}
	info, err := os.Stat(env.Data.Path)
	if err != nil || info.Size() != int64(len(body)) {
		t.Errorf("stored file wrong: %v", err)
	}

	// re-download without --overwrite must fail with FILE_EXISTS (9)
	var out2, errOut2 bytes.Buffer
	code2 := Run(context.Background(),
		[]string{"download", "https://weixin.qq.com/sph/LeakTest2", "-o", filepath.Join(work, "out.mp4")},
		strings.NewReader(""), &out2, &errOut2, deps)
	if code2 != 9 {
		t.Errorf("re-download exit %d, want 9 (FILE_EXISTS); stderr: %s", code2, errOut2.String())
	}
	assertNoSentinels(t, out2.String(), errOut2.String())
	// old file intact
	still, _ := os.Stat(env.Data.Path)
	if still == nil || still.Size() != int64(len(body)) {
		t.Errorf("existing file damaged by failed re-download")
	}
}

// The big-media memory rule: a download streams; buffers must
// not grow with the file. Smoke-check via a larger body.
func TestDownloadStreamsLargeBody(t *testing.T) {
	deps, yb, fd, md, cfgDir := leakEnv(t)
	defer yb.Close()
	defer fd.Close()
	defer md.Close()
	installCreds(t, cfgDir)

	// swap the media server for a streaming generator of ~8 MiB
	head, _ := mediaBody()
	bigSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Cookie") != "" {
			w.WriteHeader(500)
			return
		}
		w.Header().Set("Content-Type", "video/mp4")
		w.Write(head)
		chunk := make([]byte, 256<<10)
		for i := 0; i < 32; i++ {
			if _, err := w.Write(chunk); err != nil {
				return
			}
		}
	}))
	defer bigSrv.Close()
	deps.NewMediaClient = func() download.HTTPClient {
		return &http.Client{Transport: mediaTransport{bigSrv.URL}}
	}

	work := t.TempDir()
	var out, errOut bytes.Buffer
	code := Run(context.Background(),
		[]string{"download", "https://weixin.qq.com/sph/LeakBig1", "-o", filepath.Join(work, "big.mp4")},
		strings.NewReader(""), &out, &errOut, deps)
	if code != 0 {
		t.Fatalf("big download exit %d: %s", code, errOut.String())
	}
	info, _ := os.Stat(filepath.Join(work, "big.mp4"))
	want := int64(len(head)) + int64(32*(256<<10))
	if info == nil || info.Size() != want {
		t.Errorf("big file size %v, want %v", info, want)
	}
}

var _ = io.Discard
var _ = netpolicy.MaxShareURLBytes
