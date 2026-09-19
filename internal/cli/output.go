package cli

import (
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"sort"
	"time"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
)

// envelope is the single JSON object printed in --json mode.
type envelope struct {
	OK      bool       `json:"ok"`
	Command string     `json:"command,omitempty"`
	Data    any        `json:"data,omitempty"`
	Error   *errorBody `json:"error,omitempty"`
}

type errorBody struct {
	Code      string `json:"code"`
	Stage     string `json:"stage"`
	Message   string `json:"message"`
	Retryable bool   `json:"retryable"`
}

// writeJSON prints exactly one JSON object and one newline to stdout.
func writeJSON(w io.Writer, env envelope) error {
	raw, err := json.Marshal(env)
	if err != nil {
		return apperr.New(apperr.InternalError, apperr.StageArguments, "结果序列化失败")
	}
	if _, err := w.Write(append(raw, '\n')); err != nil {
		return apperr.New(apperr.IOError, apperr.StageArguments, "无法写出结果")
	}
	return nil
}

// fail reports an error: one JSON object in JSON mode, one controlled text
// line on the error stream otherwise. Returns the mapped exit code.
func fail(err *apperr.Error, jsonMode bool, stdout, errOut io.Writer) int {
	if jsonMode {
		_ = writeJSON(stdout, envelope{
			OK: false,
			Error: &errorBody{
				Code:      string(err.Code),
				Stage:     string(err.Stage),
				Message:   err.Message,
				Retryable: err.Retryable,
			},
		})
		return apperr.ExitCode(err)
	}
	fmt.Fprintf(errOut, "错误 [%s/%s]: %s\n", err.Code, err.Stage, err.Message)
	return apperr.ExitCode(err)
}

func sortStrings(s []string) { sort.Strings(s) }

// stdinIsTerminal reports whether stdin is an interactive terminal.
func stdinIsTerminal() bool {
	info, err := os.Stdin.Stat()
	if err != nil {
		return false
	}
	return info.Mode()&os.ModeCharDevice != 0
}

func execLookPath(name string) (string, error) { return exec.LookPath(name) }

// newProgressPrinter returns a stderr progress callback throttled to one line
// per second.
func newProgressPrinter(stderr io.Writer) func(sent, total int64) {
	var last time.Time
	return func(sent, total int64) {
		now := time.Now()
		if now.Sub(last) < time.Second {
			return
		}
		last = now
		if total > 0 {
			fmt.Fprintf(stderr, "\r下载中 %s / %s (%.1f%%)",
				humanBytes(sent), humanBytes(total), float64(sent)/float64(total)*100)
		} else {
			fmt.Fprintf(stderr, "\r下载中 %s", humanBytes(sent))
		}
	}
}

func humanBytes(n int64) string {
	const unit = 1024
	if n < unit {
		return fmt.Sprintf("%d B", n)
	}
	div, exp := int64(unit), 0
	for m := n / unit; m >= unit; m /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f %ciB", float64(n)/float64(div), "KMGTPE"[exp])
}

const helpText = `sph — 微信视频号本地下载工具（自用）

用法:
  sph login [--timeout 5m]                   打开专用浏览器登录，自动采集保存
  sph inspect URL [--json] [--timeout 60s]    解析分享链接，查看视频信息
  sph download URL [-o FILE.mp4] [--overwrite] [--max-bytes N] [--json]
  pbpaste | sph download --stdin              从 stdin 读取链接
  sph auth status                             查看本地凭证状态（不联网）
  sph logout                                  清除本工具保存的本地凭证
  sph auth import --stdin [--headers-file F]  故障备用：手动导入 Cookie
  sph version                                 显示版本

说明:
  - 首次使用执行 sph login：在弹出的专用浏览器中亲自登录，程序检测到
    登录成功后自动采集凭证、关闭浏览器并保存，无需输入任何内容。
  - 平时的 inspect / download 不打开浏览器、不依赖 Node。
  - 输出文件默认不覆盖同名文件；需要覆盖时加 --overwrite。
  - JSON 模式下 stdout 只输出一个 JSON 对象。
`
