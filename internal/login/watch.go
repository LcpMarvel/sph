package login

import (
	"sort"
	"strings"
)

// loginWatch is the login-detection state machine over polled cookie sets.
//
// A login page sets cookies in stages: guest cookies land while the page
// loads, then the login flow adds session cookies. Treating any growth as
// "logged in" fires while the page is still loading. The machine therefore:
//
//  1. baseline phase: wait until the guest cookie fingerprint is stable for
//     watchStablePolls polls (forced after watchMaxBaselinePolls so a chatty
//     page cannot block forever);
//  2. watch phase: trigger only when the cookie NAME set strictly grows past
//     the baseline (session cookies are new names) — value rotations alone
//     never trigger;
//  3. and the full fingerprint (names + values) has then been stable for
//     watchStablePolls polls.
type loginWatch struct {
	phase           string // "baseline" | "watch"
	baselineNames   map[string]bool
	baselinePolls   int
	lastFingerprint string
	hasLast         bool
	stableCount     int
}

const (
	watchStablePolls      = 3
	watchMaxBaselinePolls = 15

	phaseBaseline = "baseline"
	phaseWatch    = "watch"
)

// cookieEntry is the subset of a browser cookie the watch needs.
type cookieEntry struct {
	Name  string
	Value string
}

func newLoginWatch() *loginWatch {
	return &loginWatch{phase: phaseBaseline}
}

func cookieFingerprint(cookies []cookieEntry) string {
	parts := make([]string, 0, len(cookies))
	for _, c := range cookies {
		parts = append(parts, c.Name+"="+c.Value)
	}
	sort.Strings(parts)
	return strings.Join(parts, "; ")
}

func cookieNames(cookies []cookieEntry) map[string]bool {
	names := make(map[string]bool, len(cookies))
	for _, c := range cookies {
		names[c.Name] = true
	}
	return names
}

// poll feeds one poll's cookies and returns the decision for this poll:
// "baseline" when the guest baseline locked, "detected" on login, "" otherwise.
func (w *loginWatch) poll(cookies []cookieEntry) string {
	names := cookieNames(cookies)
	fp := cookieFingerprint(cookies)

	if w.phase == phaseBaseline {
		w.baselinePolls++
		if w.hasLast && fp == w.lastFingerprint {
			w.stableCount++
		} else {
			w.stableCount = 0
		}
		w.lastFingerprint = fp
		w.hasLast = true
		settled := w.stableCount >= watchStablePolls-1 && w.baselinePolls >= watchStablePolls
		if settled || w.baselinePolls >= watchMaxBaselinePolls {
			w.baselineNames = names
			w.phase = phaseWatch
			w.stableCount = 0
			w.lastFingerprint = fp
			return phaseBaseline
		}
		return ""
	}

	grew := false
	for name := range names {
		if !w.baselineNames[name] {
			grew = true
		}
	}
	if !grew {
		w.lastFingerprint = fp
		return ""
	}
	if w.hasLast && w.lastFingerprint == fp {
		w.stableCount++
	} else {
		w.lastFingerprint = fp
		w.stableCount = 1
	}
	if w.stableCount >= watchStablePolls {
		return "detected"
	}
	return ""
}
