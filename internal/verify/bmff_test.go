package verify

import (
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
)

// box builds one ISO-BMFF box: 32-bit size, 4CC type, payload.
func box(typ string, payload []byte) []byte {
	out := make([]byte, 8+len(payload))
	binary.BigEndian.PutUint32(out[0:4], uint32(len(payload)+8))
	copy(out[4:8], typ)
	copy(out[8:], payload)
	return out
}

// box64 builds one box with a 64-bit extended size.
func box64(typ string, payload []byte) []byte {
	out := make([]byte, 16+len(payload))
	binary.BigEndian.PutUint32(out[0:4], 1)
	copy(out[4:8], typ)
	binary.BigEndian.PutUint64(out[8:16], uint64(len(payload)+16))
	copy(out[16:], payload)
	return out
}

// openSizeZero appends an mdat box declared with size=0 (to EOF).
func openSizeZero(typ string, payload []byte) []byte {
	out := make([]byte, 8+len(payload))
	binary.BigEndian.PutUint32(out[0:4], 0)
	copy(out[4:8], typ)
	copy(out[8:], payload)
	return out
}

func writeTemp(t *testing.T, data []byte) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "x.mp4")
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func checkFile(t *testing.T, path string) error {
	t.Helper()
	f, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	return CheckContainer(f)
}

func TestCheckContainerValid(t *testing.T) {
	data := append(append(append([]byte{},
		box("ftyp", []byte("isom"))...),
		box("moov", make([]byte, 64))...),
		box("mdat", make([]byte, 4096))...)
	if err := checkFile(t, writeTemp(t, data)); err != nil {
		t.Errorf("valid file rejected: %v", err)
	}
}

func TestCheckContainerExtendedSize(t *testing.T) {
	data := append(append([]byte{},
		box("ftyp", []byte("isom"))...),
		box64("mdat", make([]byte, 100))...)
	// moov after an extended-size box
	data = append(data, box("moov", make([]byte, 16))...)
	if err := checkFile(t, writeTemp(t, data)); err != nil {
		t.Errorf("extended-size file rejected: %v", err)
	}
}

func TestCheckContainerSizeZeroToEOF(t *testing.T) {
	data := append(append([]byte{},
		box("ftyp", []byte("isom"))...),
		box("moov", make([]byte, 16))...)
	data = append(data, openSizeZero("mdat", make([]byte, 50))...)
	if err := checkFile(t, writeTemp(t, data)); err != nil {
		t.Errorf("size=0 file rejected: %v", err)
	}
}

func TestCheckContainerFailures(t *testing.T) {
	valid := append(append([]byte{},
		box("ftyp", []byte("isom"))...),
		box("moov", make([]byte, 16))...)

	cases := map[string][]byte{
		"empty file":              {},
		"too small":               {0, 0, 0, 1},
		"missing ftyp":            append(box("moov", nil), box("mdat", make([]byte, 8))...),
		"missing moov":            append(box("ftyp", nil), box("mdat", make([]byte, 8))...),
		"mdat no data":            append(valid, box("mdat", nil)...),
		"box overruns":            append(append(valid, []byte{0, 0, 0, 100, 'm', 'd', 'a', 't'}...), make([]byte, 4)...),
		"box smaller than header": append(valid, []byte{0, 0, 0, 4, 'f', 'r', 'e', 'e'}...),
		"truncated header":        append(valid, []byte{0, 0, 0}...),
		"html file":               []byte("<html><body>error</body></html>"),
		"zero boxes loop":         append(append(valid, []byte{0, 0, 0, 0, 'f', 'r', 'e', 'e'}...), make([]byte, 0)...),
	}
	for name, data := range cases {
		t.Run(name, func(t *testing.T) {
			err := checkFile(t, writeTemp(t, data))
			if err == nil {
				t.Fatalf("expected rejection")
			}
			if apperr.CodeOf(err) != apperr.VerifyFailed && apperr.CodeOf(err) != apperr.IOError {
				t.Errorf("unexpected code %v", apperr.CodeOf(err))
			}
		})
	}

	// a huge number of tiny boxes must be bounded
	var many []byte
	many = append(many, box("ftyp", []byte("isom"))...)
	for i := 0; i < 5000; i++ {
		many = append(many, box("free", nil)...)
	}
	if err := checkFile(t, writeTemp(t, many)); err == nil {
		t.Errorf("box count explosion must be rejected")
	}
}
