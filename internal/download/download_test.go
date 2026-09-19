package download

import (
	"context"
	"errors"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"sph/internal/apperr"
	"sph/internal/media"
)

const sentinelMediaQuery = "DO_NOT_LEAK_MEDIA_QUERY_789"

func testVideo() media.ResolvedVideo {
	return media.ResolvedVideo{
		LocalID:  "abc123def456",
		Title:    "示例 视频",
		MediaURL: "https://cdn.example.com/v.mp4?sig=" + sentinelMediaQuery,
	}
}

// synthetic MP4: ftyp + moov + mdat
func syntheticMP4(size int) []byte {
	head := box("ftyp", []byte("isom"))
	head = append(head, box("moov", make([]byte, 64))...)
	payload := make([]byte, size)
	for i := range payload {
		payload[i] = byte(i)
	}
	head = append(head, box("mdat", payload)...)
	return head
}

func box(typ string, payload []byte) []byte {
	out := make([]byte, 8+len(payload))
	out[0], out[1], out[2] = 0, 0, 0
	out[3] = byte(len(payload) + 8)
	copy(out[4:8], typ)
	copy(out[8:], payload)
	return out
}

type staticTransport struct {
	mu       sync.Mutex
	requests []*http.Request
	respond  func(r *http.Request) (*http.Response, error)
}

// Do implements HTTPClient directly (no http.Client wrapper: no redirects).
func (s *staticTransport) Do(r *http.Request) (*http.Response, error) {
	s.mu.Lock()
	clone := r.Clone(context.Background())
	if clone.Body != nil {
		clone.Body = http.NoBody
	}
	s.requests = append(s.requests, clone)
	s.mu.Unlock()
	return s.respond(r)
}

func bodyResp(status int, body []byte, contentLength int64) *http.Response {
	if contentLength < 0 {
		contentLength = int64(len(body))
	}
	return &http.Response{
		StatusCode:    status,
		Header:        http.Header{},
		Body:          io.NopCloser(strings.NewReader(string(body))),
		ContentLength: contentLength,
	}
}

func okVerify(ctx context.Context, path string) (string, error) { return "container", nil }

func tmpWorkDir(t *testing.T) string {
	t.Helper()
	return t.TempDir()
}

func runDownload(t *testing.T, opts Options, video media.ResolvedVideo) (Result, error) {
	t.Helper()
	opts.Verify = okVerify
	dl, err := New(video, opts)
	if err != nil {
		return Result{}, err
	}
	return dl.Run(context.Background())
}

func TestDownloadHappyPathAutoNamed(t *testing.T) {
	dir := tmpWorkDir(t)
	data := syntheticMP4(1024)
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		return bodyResp(200, data, int64(len(data))), nil
	}}
	res, err := runDownload(t, Options{WorkDir: dir, MaxBytes: 1 << 20, HTTP: tp}, testVideo())
	if err != nil {
		t.Fatalf("download: %v", err)
	}
	if res.Path != filepath.Join(dir, "示例 视频_abc123def456.mp4") {
		t.Errorf("auto name wrong: %s", res.Path)
	}
	info, err := os.Stat(res.Path)
	if err != nil {
		t.Fatal(err)
	}
	if info.Size() != int64(len(data)) || info.Mode().Perm() != 0o600 {
		t.Errorf("stored file wrong: size=%d perm=%o", info.Size(), info.Mode().Perm())
	}
	if res.Bytes != int64(len(data)) || res.SHA256 == "" || res.Verification != "container" {
		t.Errorf("result wrong: %+v", res)
	}
	// no temp files left behind
	entries, _ := os.ReadDir(dir)
	if len(entries) != 1 {
		for _, e := range entries {
			t.Logf("leftover: %s", e.Name())
		}
		t.Errorf("expected exactly the final file, got %d entries", len(entries))
	}
	// media request carries no session material and preserves the signed URL
	req := tp.requests[0]
	if req.Header.Get("Cookie") != "" || req.Header.Get("Referer") != "https://channels.weixin.qq.com/" {
		t.Errorf("media headers wrong: cookie=%q referer=%q", req.Header.Get("Cookie"), req.Header.Get("Referer"))
	}
	if got := req.URL.String(); got != testVideo().MediaURL {
		t.Errorf("media URL rewritten: %s", got)
	}
}

func TestDownloadExplicitOutputValidation(t *testing.T) {
	dir := tmpWorkDir(t)
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		return bodyResp(200, syntheticMP4(10), -1), nil
	}}
	// non-mp4 extension
	_, err := runDownload(t, Options{OutputPath: filepath.Join(dir, "a.txt"), HTTP: tp}, testVideo())
	if apperr.CodeOf(err) != apperr.InvalidArgument {
		t.Errorf("want INVALID_ARGUMENT, got %v", err)
	}
	// parent missing
	_, err = runDownload(t, Options{OutputPath: filepath.Join(dir, "nope", "a.mp4"), HTTP: tp}, testVideo())
	if apperr.CodeOf(err) != apperr.InvalidArgument {
		t.Errorf("missing parent: %v", apperr.CodeOf(err))
	}
	// target is a directory
	os.Mkdir(filepath.Join(dir, "d.mp4"), 0o755)
	_, err = runDownload(t, Options{OutputPath: filepath.Join(dir, "d.mp4"), HTTP: tp}, testVideo())
	if apperr.CodeOf(err) != apperr.InvalidArgument {
		t.Errorf("dir target: %v", apperr.CodeOf(err))
	}
	// target is a symlink
	os.Symlink(filepath.Join(dir, "real.mp4"), filepath.Join(dir, "link.mp4"))
	_, err = runDownload(t, Options{OutputPath: filepath.Join(dir, "link.mp4"), HTTP: tp}, testVideo())
	if apperr.CodeOf(err) != apperr.InvalidArgument {
		t.Errorf("symlink target: %v", apperr.CodeOf(err))
	}
}

func TestDownloadNoClobberByDefault(t *testing.T) {
	dir := tmpWorkDir(t)
	target := filepath.Join(dir, "out.mp4")
	old := []byte("OLD FILE CONTENT")
	os.WriteFile(target, old, 0o600)
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		return bodyResp(200, syntheticMP4(64), -1), nil
	}}
	// detected before any network call
	_, err := runDownload(t, Options{OutputPath: target, HTTP: tp}, testVideo())
	if apperr.CodeOf(err) != apperr.FileExists {
		t.Fatalf("want FILE_EXISTS, got %v", err)
	}
	if len(tp.requests) != 0 {
		t.Errorf("conflict should be caught before networking")
	}
	got, _ := os.ReadFile(target)
	if string(got) != string(old) {
		t.Errorf("old file damaged")
	}

	// overwrite succeeds and replaces content atomically
	res, err := runDownload(t, Options{OutputPath: target, Overwrite: true, HTTP: tp, MaxBytes: 1 << 20}, testVideo())
	if err != nil {
		t.Fatalf("overwrite: %v", err)
	}
	if res.Path != target {
		t.Errorf("path %s", res.Path)
	}
}

func TestDownloadHTTPFailures(t *testing.T) {
	dir := tmpWorkDir(t)
	cases := []struct {
		name     string
		respond  func() (*http.Response, error)
		wantCode apperr.Code
	}{
		{"404", func() (*http.Response, error) { return bodyResp(404, []byte("x"), 1), nil }, apperr.DownloadFailed},
		{"403", func() (*http.Response, error) { return bodyResp(403, []byte("x"), 1), nil }, apperr.DownloadFailed},
		{"206", func() (*http.Response, error) { return bodyResp(206, syntheticMP4(32), -1), nil }, apperr.DownloadFailed},
		{"500", func() (*http.Response, error) { return bodyResp(500, []byte("x"), 1), nil }, apperr.DownloadFailed},
		{"html body", func() (*http.Response, error) {
			return bodyResp(200, []byte("<html><body>err</body></html>"), -1), nil
		}, apperr.DownloadFailed},
		{"json body", func() (*http.Response, error) {
			return bodyResp(200, []byte(`{"error":"nope"}`), -1), nil
		}, apperr.DownloadFailed},
		{"xml body", func() (*http.Response, error) {
			return bodyResp(200, []byte("<?xml version=\"1.0\"?><e/>"), -1), nil
		}, apperr.DownloadFailed},
		{"hls playlist", func() (*http.Response, error) {
			return bodyResp(200, []byte("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nx.m3u8"), -1), nil
		}, apperr.UnsupportedMedia},
		{"dash manifest", func() (*http.Response, error) {
			return bodyResp(200, []byte("<MPD xmlns=\"urn:mpeg:dash\">x</MPD>"), -1), nil
		}, apperr.UnsupportedMedia},
		{"empty body", func() (*http.Response, error) { return bodyResp(200, nil, -1), nil }, apperr.DownloadFailed},
		{"length mismatch", func() (*http.Response, error) {
			data := syntheticMP4(100)
			return bodyResp(200, data, int64(len(data)+7)), nil
		}, apperr.DownloadFailed},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) { return tc.respond() }}
			_, err := runDownload(t, Options{OutputPath: filepath.Join(dir, "o.mp4"), HTTP: tp, MaxBytes: 1 << 20}, testVideo())
			if apperr.CodeOf(err) != tc.wantCode {
				t.Errorf("got %v, want %s", err, tc.wantCode)
			}
			entries, _ := os.ReadDir(dir)
			if len(entries) != 0 {
				t.Errorf("failure must leave no temp files, found %d", len(entries))
			}
		})
	}
}

func TestDownloadTooLarge(t *testing.T) {
	dir := tmpWorkDir(t)
	// known Content-Length over the cap: rejected before reading
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		data := syntheticMP4(4096)
		return bodyResp(200, data, int64(len(data))), nil
	}}
	_, err := runDownload(t, Options{OutputPath: filepath.Join(dir, "o.mp4"), HTTP: tp, MaxBytes: 1024}, testVideo())
	if apperr.CodeOf(err) != apperr.DownloadTooLarge {
		t.Errorf("want DOWNLOAD_TOO_LARGE (CL), got %v", err)
	}
	// unknown length: enforced while streaming
	tp2 := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		resp := bodyResp(200, syntheticMP4(4096), -1)
		resp.ContentLength = -1
		return resp, nil
	}}
	_, err = runDownload(t, Options{OutputPath: filepath.Join(dir, "o.mp4"), HTTP: tp2, MaxBytes: 1024}, testVideo())
	if apperr.CodeOf(err) != apperr.DownloadTooLarge {
		t.Errorf("want DOWNLOAD_TOO_LARGE (stream), got %v", err)
	}
}

// A transfer that dies midway must be retried ONCE from scratch: the second
// attempt succeeds and the stored hash covers the whole file.
func TestDownloadRetriesOnceFromScratch(t *testing.T) {
	dir := tmpWorkDir(t)
	full := syntheticMP4(2048)
	attempt := 0
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		attempt++
		if attempt == 1 {
			return &http.Response{
				StatusCode: 200,
				Header:     http.Header{},
				Body:       io.NopCloser(&halfBrokenReader{data: full, failAfter: 100}),
			}, nil
		}
		return bodyResp(200, full, int64(len(full))), nil
	}}
	res, err := runDownload(t, Options{OutputPath: filepath.Join(dir, "o.mp4"), HTTP: tp, MaxBytes: 1 << 20}, testVideo())
	if err != nil {
		t.Fatalf("retry from scratch should succeed: %v", err)
	}
	if attempt != 2 {
		t.Errorf("attempts=%d, want 2", attempt)
	}
	stored, _ := os.ReadFile(res.Path)
	if len(stored) != len(full) {
		t.Errorf("stored %d bytes, want %d (retry must not append)", len(stored), len(full))
	}
}

type halfBrokenReader struct {
	data      []byte
	failAfter int
	pos       int
	failed    bool
}

func (h *halfBrokenReader) Read(p []byte) (int, error) {
	if h.failed {
		return 0, errors.New("connection reset by peer")
	}
	if h.pos >= h.failAfter {
		h.failed = true
		return 0, errors.New("connection reset by peer")
	}
	n := copy(p, h.data[h.pos:min(h.pos+len(p), h.failAfter)])
	h.pos += n
	if n == 0 {
		return 0, errors.New("connection reset by peer")
	}
	return n, nil
}

func TestDownloadDoubleFailureGivesUp(t *testing.T) {
	dir := tmpWorkDir(t)
	attempt := 0
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		attempt++
		return &http.Response{
			StatusCode: 200,
			Header:     http.Header{},
			Body:       io.NopCloser(&halfBrokenReader{data: syntheticMP4(512), failAfter: 10}),
		}, nil
	}}
	_, err := runDownload(t, Options{OutputPath: filepath.Join(dir, "o.mp4"), HTTP: tp, MaxBytes: 1 << 20}, testVideo())
	if apperr.CodeOf(err) != apperr.NetworkError {
		t.Errorf("want NETWORK_ERROR after retry exhaustion, got %v", err)
	}
	if attempt != 2 {
		t.Errorf("must stop after 2 attempts, got %d", attempt)
	}
	entries, _ := os.ReadDir(dir)
	if len(entries) != 0 {
		t.Errorf("temp files left: %d", len(entries))
	}
}

func TestDownloadCancelCleansTemp(t *testing.T) {
	dir := tmpWorkDir(t)
	ctx, cancel := context.WithCancel(context.Background())
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		cancel()
		return &http.Response{
			StatusCode: 200,
			Header:     http.Header{},
			Body:       io.NopCloser(&cancelReader{ctx: ctx, data: syntheticMP4(4096)}),
		}, nil
	}}
	dl, err := New(testVideo(), Options{OutputPath: filepath.Join(dir, "o.mp4"), HTTP: tp, MaxBytes: 1 << 20, Verify: okVerify})
	if err != nil {
		t.Fatal(err)
	}
	_, err = dl.Run(ctx)
	if apperr.CodeOf(err) != apperr.Cancelled {
		t.Errorf("want CANCELLED, got %v", err)
	}
	entries, _ := os.ReadDir(dir)
	if len(entries) != 0 {
		t.Errorf("cancel must clean temp files, left %d", len(entries))
	}
}

type cancelReader struct {
	ctx      context.Context
	data     []byte
	pos      int
	canceled bool
}

func (c *cancelReader) Read(p []byte) (int, error) {
	if c.pos >= len(c.data) {
		return 0, io.EOF
	}
	if c.canceled {
		return 0, io.ErrClosedPipe
	}
	n := copy(p, c.data[c.pos:])
	c.pos += n
	if c.ctx.Err() != nil {
		c.canceled = true
	}
	return n, nil
}

func TestDownloadVerifyFailureKeepsNoFile(t *testing.T) {
	dir := tmpWorkDir(t)
	data := syntheticMP4(512)
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		return bodyResp(200, data, int64(len(data))), nil
	}}
	opts := Options{
		OutputPath: filepath.Join(dir, "o.mp4"),
		HTTP:       tp,
		MaxBytes:   1 << 20,
		Verify: func(ctx context.Context, path string) (string, error) {
			return "", apperr.New(apperr.VerifyFailed, apperr.StageVerify, "bad container")
		},
	}
	dl, _ := New(testVideo(), opts)
	_, err := dl.Run(context.Background())
	if apperr.CodeOf(err) != apperr.VerifyFailed {
		t.Errorf("want VERIFY_FAILED, got %v", err)
	}
	entries, _ := os.ReadDir(dir)
	if len(entries) != 0 {
		t.Errorf("failed verification must leave no files, left %d", len(entries))
	}
}

// Unknown Content-Length must still stream fully and verify.
func TestDownloadUnknownContentLength(t *testing.T) {
	dir := tmpWorkDir(t)
	data := syntheticMP4(300)
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		resp := bodyResp(200, data, -1)
		resp.ContentLength = -1
		resp.Header.Set("Content-Length", "") // chunked style
		return resp, nil
	}}
	res, err := runDownload(t, Options{OutputPath: filepath.Join(dir, "o.mp4"), HTTP: tp, MaxBytes: 1 << 20}, testVideo())
	if err != nil {
		t.Fatalf("unknown length download: %v", err)
	}
	if res.Bytes != int64(len(data)) {
		t.Errorf("bytes %d, want %d", res.Bytes, len(data))
	}
}

func TestMaxBytesValidation(t *testing.T) {
	dir := tmpWorkDir(t)
	tp := &staticTransport{respond: func(r *http.Request) (*http.Response, error) {
		return bodyResp(200, syntheticMP4(10), -1), nil
	}}
	if _, err := New(testVideo(), Options{OutputPath: filepath.Join(dir, "o.mp4"), HTTP: tp, MaxBytes: -5}); err == nil {
		t.Errorf("negative max-bytes must be rejected in New (falls back), or at least not panic")
	}
}
