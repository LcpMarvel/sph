package verify

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"time"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
)

// FFProbeTimeout caps ffprobe inside the parent command context.
const FFProbeTimeout = 20 * time.Second

// Verification names for the download result DTO.
const (
	VerificationFFProbe   = "ffprobe"
	VerificationContainer = "container"
)

// ffprobeOutput is the subset of `ffprobe -show_streams -show_format -of json`
// this tool reads.
type ffprobeOutput struct {
	Streams []struct {
		CodecType string `json:"codec_type"`
	} `json:"streams"`
}

// FFProbeRunner runs the ffprobe binary with the given args; injectable for
// tests so tests never shell out.
type FFProbeRunner func(ctx context.Context, args ...string) ([]byte, error)

// DefaultFFProbeRunner finds ffprobe on PATH and executes it without a shell.
func DefaultFFProbeRunner(ctx context.Context, args ...string) ([]byte, error) {
	path, err := exec.LookPath("ffprobe")
	if err != nil {
		return nil, err
	}
	cmd := exec.CommandContext(ctx, path, args...)
	return cmd.Output()
}

// Verify checks a downloaded file: container check always; ffprobe when it is
// installed. It returns "ffprobe" or "container" so callers can express what
// was actually verified.
func Verify(ctx context.Context, localPath string) (string, error) {
	return VerifyWith(ctx, DefaultFFProbeRunner, localPath)
}

// VerifyWith is Verify with an injectable ffprobe runner for offline tests.
func VerifyWith(ctx context.Context, runner FFProbeRunner, localPath string) (string, error) {
	f, err := openFile(localPath)
	if err != nil {
		return "", err
	}
	defer f.Close()
	if err := CheckContainer(f); err != nil {
		return "", err
	}
	return runFFProbe(ctx, runner, localPath)
}

// HasFFProbe reports whether ffprobe is available on PATH.
func HasFFProbe() bool {
	_, err := exec.LookPath("ffprobe")
	return err == nil
}

func runFFProbe(ctx context.Context, runner FFProbeRunner, localPath string) (string, error) {
	probeCtx, cancel := context.WithTimeout(ctx, FFProbeTimeout)
	defer cancel()
	abs, err := filepath.Abs(localPath)
	if err != nil {
		abs = localPath
	}
	out, err := runner(probeCtx, "-v", "error", "-show_streams", "-show_format", "-of", "json", abs)
	if err != nil {
		if execErr := ctx.Err(); execErr != nil {
			return "", apperr.Wrap(execErr, apperr.Cancelled, apperr.StageVerify, "验证已取消")
		}
		// ffprobe missing is the only silent downgrade; any installed ffprobe
		// that fails is a VERIFY_FAILED.
		if errors.Is(err, exec.ErrNotFound) {
			return VerificationContainer, nil
		}
		return "", apperr.New(apperr.VerifyFailed, apperr.StageVerify, "ffprobe 检查未通过，文件可能不是可播放的视频")
	}
	var parsed ffprobeOutput
	if err := json.Unmarshal(out, &parsed); err != nil {
		return "", apperr.New(apperr.VerifyFailed, apperr.StageVerify, "无法解析 ffprobe 输出")
	}
	for _, s := range parsed.Streams {
		if s.CodecType == "video" {
			return VerificationFFProbe, nil
		}
	}
	return "", apperr.New(apperr.VerifyFailed, apperr.StageVerify, "ffprobe 未检测到视频流")
}

// openFile opens a regular file for the container check, refusing symlinks
// and directories.
func openFile(path string) (*os.File, error) {
	info, err := os.Lstat(path)
	if err != nil {
		return nil, apperr.Wrap(err, apperr.IOError, apperr.StageVerify, "无法访问已下载文件")
	}
	if !info.Mode().IsRegular() {
		return nil, apperr.New(apperr.IOError, apperr.StageVerify, "下载结果不是普通文件")
	}
	f, err := os.Open(path)
	if err != nil {
		return nil, apperr.Wrap(err, apperr.IOError, apperr.StageVerify, "无法打开已下载文件")
	}
	return f, nil
}
