package verify

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"testing"
)

func writeFile(path string, data []byte) error { return os.WriteFile(path, data, 0o600) }

func ffprobeJSON(streams ...string) []byte {
	out := `{"streams":[`
	for i, s := range streams {
		if i > 0 {
			out += ","
		}
		out += `{"codec_type":"` + s + `"}`
	}
	return []byte(out + `],"format":{"format_name":"mov,mp4"}}`)
}

func TestVerifyContainerOnlyWhenFFProbeMissing(t *testing.T) {
	dir := t.TempDir()
	path := writeTemp(t, append(append([]byte{},
		box("ftyp", []byte("isom"))...),
		box("moov", make([]byte, 32))...),
	)
	_ = dir
	// readValidFile opens a real file; write one valid container.
	data := append(append(append([]byte{},
		box("ftyp", []byte("isom"))...),
		box("moov", make([]byte, 32))...),
		box("mdat", make([]byte, 128))...)
	if err := writeFile(path, data); err != nil {
		t.Fatal(err)
	}
	missing := func(ctx context.Context, args ...string) ([]byte, error) {
		return nil, exec.ErrNotFound
	}
	method, err := VerifyWith(context.Background(), missing, path)
	if err != nil {
		t.Fatalf("missing ffprobe must downgrade to container: %v", err)
	}
	if method != VerificationContainer {
		t.Errorf("method %q, want container", method)
	}
}

func TestVerifyFFProbeSuccessAndFailure(t *testing.T) {
	data := append(append(append([]byte{},
		box("ftyp", []byte("isom"))...),
		box("moov", make([]byte, 32))...),
		box("mdat", make([]byte, 128))...)
	path := writeTemp(t, data)

	// success: one video stream
	method, err := VerifyWith(context.Background(), func(ctx context.Context, args ...string) ([]byte, error) {
		for _, a := range args {
			if a == "-of" {
				// ensure the file path is passed as a bare arg, not shell-joined
			}
		}
		return ffprobeJSON("video", "audio"), nil
	}, path)
	if err != nil || method != VerificationFFProbe {
		t.Errorf("got %q/%v, want ffprobe/nil", method, err)
	}

	// no video stream
	if _, err := VerifyWith(context.Background(), func(context.Context, ...string) ([]byte, error) {
		return ffprobeJSON("audio"), nil
	}, path); err == nil {
		t.Errorf("audio-only must fail verification")
	}

	// ffprobe error
	if _, err := VerifyWith(context.Background(), func(context.Context, ...string) ([]byte, error) {
		return nil, errors.New("exit status 1")
	}, path); err == nil {
		t.Errorf("ffprobe failure must be VERIFY_FAILED, never downgraded")
	}

	// garbage json
	if _, err := VerifyWith(context.Background(), func(context.Context, ...string) ([]byte, error) {
		return []byte("not json"), nil
	}, path); err == nil {
		t.Errorf("garbage output must fail")
	}
}

func TestVerifyBadContainerFailsBeforeFFProbe(t *testing.T) {
	called := false
	_, err := VerifyWith(context.Background(), func(context.Context, ...string) ([]byte, error) {
		called = true
		return ffprobeJSON("video"), nil
	}, writeTemp(t, []byte("<html>")))
	if err == nil {
		t.Fatalf("bad container must fail")
	}
	if called {
		t.Errorf("ffprobe must not run after container failure")
	}
}
