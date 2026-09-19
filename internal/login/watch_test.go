package login

import "testing"

func guestCookies() []cookieEntry {
	return []cookieEntry{{Name: "guest_a", Value: "1"}, {Name: "guest_b", Value: "2"}}
}

func withCookies(base []cookieEntry, extra ...cookieEntry) []cookieEntry {
	out := append([]cookieEntry{}, base...)
	return append(out, extra...)
}

func TestWatchGuestPageLoadingIsNotLogin(t *testing.T) {
	w := newLoginWatch()
	results := []string{
		w.poll([]cookieEntry{{Name: "guest_a", Value: "1"}}),
		w.poll(guestCookies()),
		w.poll(withCookies(guestCookies(), cookieEntry{Name: "guest_c", Value: "3"})),
	}
	for i, r := range results {
		if r != "" {
			t.Errorf("poll %d: guest cookie landing must not decide anything, got %q", i+1, r)
		}
	}
	if w.phase != phaseBaseline {
		t.Errorf("still loading page must stay in baseline phase, got %s", w.phase)
	}
}

func TestWatchBaselineLocksThenGrowthAndStabilityDetects(t *testing.T) {
	w := newLoginWatch()
	decisions := []string{
		w.poll(guestCookies()),
		w.poll(guestCookies()),
		w.poll(guestCookies()),
	}
	if decisions[0] != "" || decisions[1] != "" || decisions[2] != phaseBaseline {
		t.Fatalf("baseline lock sequence wrong: %v", decisions)
	}
	if w.phase != phaseWatch {
		t.Fatalf("expected watch phase after baseline lock")
	}
	loggedIn := withCookies(guestCookies(), cookieEntry{Name: "session", Value: "s1"})
	if r := w.poll(loggedIn); r != "" {
		t.Fatalf("first growth poll must not detect, got %q", r)
	}
	if r := w.poll(loggedIn); r != "" {
		t.Fatalf("second stable poll must not detect yet, got %q", r)
	}
	if r := w.poll(loggedIn); r != "detected" {
		t.Fatalf("third stable poll must detect, got %q", r)
	}
}

func TestWatchValueRotationNeverTriggers(t *testing.T) {
	w := newLoginWatch()
	for i := 0; i < watchStablePolls; i++ {
		w.poll(guestCookies())
	}
	if w.phase != phaseWatch {
		t.Fatalf("baseline should have locked")
	}
	for i := 0; i < 20; i++ {
		rotated := []cookieEntry{{Name: "guest_a", Value: "v" + string(rune('a'+i%26))}, {Name: "guest_b", Value: "2"}}
		if r := w.poll(rotated); r != "" {
			t.Fatalf("value rotation without new names must never trigger, got %q", r)
		}
	}
}

func TestWatchGrowthMustSettleBeforeDetecting(t *testing.T) {
	w := newLoginWatch()
	for i := 0; i < watchStablePolls; i++ {
		w.poll(guestCookies())
	}
	// login lands cookies one per poll: fingerprint keeps changing
	if r := w.poll(withCookies(guestCookies(), cookieEntry{Name: "s1", Value: "a"})); r != "" {
		t.Fatalf("unstable growth must not detect, got %q", r)
	}
	settled := withCookies(guestCookies(),
		cookieEntry{Name: "s1", Value: "a"},
		cookieEntry{Name: "s2", Value: "b"},
	)
	if r := w.poll(settled); r != "" {
		t.Fatalf("first observation of settled set must not detect, got %q", r)
	}
	if r := w.poll(settled); r != "" {
		t.Fatalf("second observation must not detect, got %q", r)
	}
	if r := w.poll(settled); r != "detected" {
		t.Fatalf("third observation must detect, got %q", r)
	}
}

func TestWatchForcesBaselineAfterChattyPage(t *testing.T) {
	w := newLoginWatch()
	decision := ""
	for i := 0; i < watchMaxBaselinePolls && decision != phaseBaseline; i++ {
		chatty := []cookieEntry{
			{Name: "guest_a", Value: "t" + string(rune('a'+i%26))},
			{Name: "guest_b", Value: "2"},
		}
		decision = w.poll(chatty)
	}
	if decision != phaseBaseline {
		t.Fatalf("baseline must be forced after %d polls", watchMaxBaselinePolls)
	}
	loggedIn := withCookies(guestCookies(), cookieEntry{Name: "session", Value: "x"})
	w.poll(loggedIn)
	w.poll(loggedIn)
	if r := w.poll(loggedIn); r != "detected" {
		t.Fatalf("login after forced baseline must still detect, got %q", r)
	}
}

func TestWatchEmptyCookieListTolerated(t *testing.T) {
	w := newLoginWatch()
	for i := 0; i < watchStablePolls; i++ {
		w.poll(nil)
	}
	if w.phase != phaseWatch {
		t.Fatalf("empty guest baseline should lock")
	}
	first := []cookieEntry{{Name: "first", Value: "1"}}
	for i := 0; i < watchStablePolls-1; i++ {
		if r := w.poll(first); r != "" {
			t.Fatalf("poll %d must not detect, got %q", i+1, r)
		}
	}
	if r := w.poll(first); r != "detected" {
		t.Fatalf("stable new cookies must detect, got %q", r)
	}
}
