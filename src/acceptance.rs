//! Phase 9 acceptance test for the embedded-terminal feature.
//!
//! Drives a real `App` against a real `tmux` server (running on an
//! isolated socket / TMUX_TMPDIR), exercising the full keystroke
//! pipeline: outer crossterm event → App::handle_key → chord_step →
//! translate_key → PTY write → tmux attach client → tmux server →
//! bash / vim → tmux capture → vt100 parser → grid inspection.
//!
//! A passing run proves the entire feature works end-to-end without
//! needing a human to drive a real terminal. This is the bar Phase 11
//! (PR + merge) won't cross until it's green.

#![cfg(test)]
#![cfg(unix)]

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

use crate::app::{App, AppState, SessionAction};
use crate::test_harness::{
    acquire_acceptance_lock, poll_for_capture_contains, poll_until, IsolatedTmux,
};
use crate::tmux::local::LocalTmux;
use crate::tmux::TmuxBackend;

// ───────────── key event constructors ─────────────

fn key_press(code: KeyCode, m: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: m,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn k(c: char) -> KeyEvent {
    key_press(KeyCode::Char(c), KeyModifiers::NONE)
}

fn k_ctrl(c: char) -> KeyEvent {
    key_press(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// The exit-chord prefix as a synthesized KeyEvent. Ctrl+Space is the
/// Danish-friendly default; crossterm reports it as Char(' ') + CTRL.
fn k_ctrl_space() -> KeyEvent {
    key_press(KeyCode::Char(' '), KeyModifiers::CONTROL)
}

fn k_enter() -> KeyEvent {
    key_press(KeyCode::Enter, KeyModifiers::NONE)
}

fn k_esc() -> KeyEvent {
    key_press(KeyCode::Esc, KeyModifiers::NONE)
}

fn k_tab() -> KeyEvent {
    key_press(KeyCode::Tab, KeyModifiers::NONE)
}

fn k_down() -> KeyEvent {
    key_press(KeyCode::Down, KeyModifiers::NONE)
}

// ───────────── grid polling ─────────────

/// Wait for the embedded vt100 grid to contain `needle`, polling at
/// 20ms cadence. Returns the matching grid contents on success or an
/// error (with the last grid snapshot) on timeout — the snapshot
/// helps debug what actually showed up.
fn poll_for_embedded_grid_contains(
    app: &App,
    needle: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut last = String::new();
    let ok = poll_until(timeout, || {
        let Some(et) = app.embedded_term.as_ref() else {
            return false;
        };
        let parser = et.parser();
        let Ok(p) = parser.lock() else {
            return false;
        };
        last = p.screen().contents();
        last.contains(needle)
    });
    if ok {
        Ok(last)
    } else {
        Err(format!(
            "timed out after {:?} waiting for {:?} in embedded grid; last:\n{}",
            timeout, needle, last
        ))
    }
}

/// Type out a string by pressing each character one at a time. Mirrors
/// what a real keyboard would deliver — important because it forces
/// the chord state machine and key translator through every byte.
fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        app.handle_key(k(c));
    }
}

// ───────────── the big one ─────────────

/// Phase 0–9 of the acceptance plan. Everything that's needed to
/// declare the feature working end-to-end.
#[test]
fn acceptance_full_embed_lifecycle() {
    let _lock = acquire_acceptance_lock();

    // ── Phase 0: setup ────────────────────────────────────────────
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("testsess", 80, 24).expect("new-session");
    // Confirm the prompt is up before we let App refresh — otherwise
    // our refresh might race the bash prompt and miss the session.
    let _ = poll_for_capture_contains(
        &tmux,
        "testsess",
        "$ ",
        Duration::from_secs(2),
    )
    .expect("test session should produce '$ ' prompt");

    // ── Phase 1: build App, enable preview, cursor on testsess ───
    let mut app = App::new();
    // App.refresh ran inside App::new; the test session should be
    // present in the tree.
    let has_testsess = app
        .tree
        .sessions
        .iter()
        .any(|s| s.raw_name == "testsess");
    assert!(
        has_testsess,
        "App tree should contain testsess after refresh; tree: {:?}",
        app.tree.sessions.iter().map(|s| &s.raw_name).collect::<Vec<_>>()
    );

    // Enable the preview pane (simulate `p`).
    app.handle_key(k('p'));
    assert!(app.preview_pane_enabled, "p should enable preview pane");

    // Navigate the cursor onto the testsess row. With one loose
    // session and no folders, visible_rows is [Session(0), NewSession]
    // and selected_index starts at 0 — but defend against slight
    // tree-layout changes by stepping until we land on the row.
    let mut tries = 0;
    loop {
        let target = app.preview_target();
        if target.as_ref().map(|k| k.name.as_str()) == Some("testsess") {
            break;
        }
        if tries > 10 {
            panic!(
                "cursor did not land on testsess after 10 down-presses; \
                 visible rows: {:?}",
                app.tree.visible_rows()
            );
        }
        app.handle_key(k_down());
        tries += 1;
    }

    // ── Phase 2: Tab into embedded mode ───────────────────────────
    app.handle_key(k_tab());
    assert!(
        app.embedded_active(),
        "Tab on a session row should enter embedded mode"
    );

    // Wait for the embedded `tmux attach` to render the prompt.
    // We're attaching to a session bash already created.
    poll_for_embedded_grid_contains(&app, "$ ", Duration::from_secs(3))
        .expect("embedded grid should show prompt");

    // ── Phase 3: send echo hello, prove it round-trips ────────────
    type_str(&mut app, "echo hello");
    app.handle_key(k_enter());
    poll_for_embedded_grid_contains(&app, "hello", Duration::from_secs(3))
        .expect("echo hello should appear in embedded grid");

    // ── Phase 4: vim edit + Esc passthrough ───────────────────────
    let vim_path = format!("/tmp/ade-acceptance-{}.txt", std::process::id());
    let _ = std::fs::remove_file(&vim_path);
    type_str(&mut app, &format!("vim {}", vim_path));
    app.handle_key(k_enter());
    // Wait for vim to load — the `~` empty-line markers are a
    // reliable signal vim has redrawn. 10s timeout is generous for
    // slow CI VMs / cold first-launch on macOS where binary
    // signature checks add seconds (Codex Phase-9 review).
    poll_for_embedded_grid_contains(&app, "~", Duration::from_secs(10))
        .expect("vim should load and show empty-line markers");
    // Insert mode + content + Esc + :wq + Enter.
    app.handle_key(k('i'));
    type_str(&mut app, "the quick brown fox");
    app.handle_key(k_esc());
    type_str(&mut app, ":wq");
    app.handle_key(k_enter());
    // Wait for prompt to come back (vim exited).
    poll_for_embedded_grid_contains(&app, "$ ", Duration::from_secs(10))
        .expect("after :wq the bash prompt should return");
    // The file should exist with our text.
    let body = std::fs::read_to_string(&vim_path).expect("vim should have written file");
    assert_eq!(
        body.trim(),
        "the quick brown fox",
        "vim wrote: {:?}",
        body
    );
    let _ = std::fs::remove_file(&vim_path);

    // ── Phase 5: resize forwarding ────────────────────────────────
    // EmbeddedTerm exposes resize as &self; call directly to bypass
    // the UI render path (TestBackend would be heavier than needed).
    {
        let et = app.embedded_term.as_ref().expect("embedded alive");
        et.resize(30, 100).expect("resize embedded PTY");
    }
    // tmux's session pane should match the new dimensions (single
    // client → smallest = ours). Allow a brief settling window.
    let resize_ok = poll_until(Duration::from_secs(2), || {
        match tmux.pane_size("testsess") {
            Ok((w, h)) => w == 100 && h == 30,
            Err(_) => false,
        }
    });
    assert!(
        resize_ok,
        "tmux pane should report 100x30 after resize, got {:?}",
        tmux.pane_size("testsess")
    );

    // ── Phase 6: exit via the chord ───────────────────────────────
    app.handle_key(k_ctrl_space()); // Ctrl+Space — the Danish-friendly chord prefix
    assert!(
        app.embedded_chord_pending(),
        "first chord byte should arm the chord state"
    );
    app.handle_key(k(' '));
    assert!(
        !app.embedded_active(),
        "chord then Space should exit embedded mode"
    );
    assert!(!app.embedded_chord_pending());

    // ── Phase 7: session survived the embed/detach cycle ─────────
    assert!(
        tmux.has_session("testsess"),
        "tmux session should still be alive after embedded detach"
    );
    let cap_after = tmux
        .capture("testsess")
        .expect("capture-pane after detach");
    // Strong assertion (Codex Phase-9): require `hello` from Phase 3
    // to be visible in the post-detach pane snapshot, not just any
    // prompt-shaped string. Proves we're inspecting the same living
    // session, not a fresh one.
    assert!(
        cap_after.contains("hello"),
        "session pane should retain Phase-3 'hello' output after \
         embedded detach; cap:\n{}",
        cap_after
    );

    // ── Phase 8: re-embed continuity ──────────────────────────────
    app.handle_key(k_tab());
    assert!(
        app.embedded_active(),
        "second Tab should re-enter embedded mode"
    );
    // Strong continuity check: the embedded grid after re-attach
    // should still contain the `hello` echo output we left there
    // before exiting — proves we're attaching to the same living
    // session, not a fresh one (Codex Phase-9 review).
    poll_for_embedded_grid_contains(&app, "hello", Duration::from_secs(5))
        .expect(
            "re-embed should show the previous 'hello' output \
             — proves attach is to the same session",
        );

    // ── Phase 9: cleanup ──────────────────────────────────────────
    // Exit verb is `Ctrl+Space` then plain `Space` (previously `q`) — the
    // chord change lives in `embedded_term::chord_step`. See the comment
    // there for why `Space` is the safer terminator than `q`.
    app.handle_key(k_ctrl_space());
    app.handle_key(k(' '));
    assert!(!app.embedded_active());
    drop(app);
    drop(tmux);
}

// ───────────── supporting acceptance tests (Phase 10) ─────────────

/// Mouse passthrough end-to-end: a scroll-up event delivered to the
/// embedded panel should reach tmux as an SGR-1006 sequence and put
/// the pane into copy-mode (since mouse is on for this fixture).
/// Proves the full path: handle_mouse → translate_mouse → PTY write
/// → tmux client → tmux server → mouse handler → enters copy-mode.
#[test]
fn acceptance_mouse_scroll_enters_copy_mode() {
    use crossterm::event::{MouseEvent, MouseEventKind};

    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("mousesess", 80, 24).expect("new-session");
    // Mouse on so tmux interprets scroll → copy-mode.
    tmux.set_option("mouse", "on").expect("mouse on");

    let mut app = App::new();
    app.handle_key(k('p'));
    let mut tries = 0;
    while app.preview_target().as_ref().map(|k| k.name.as_str()) != Some("mousesess") {
        if tries > 10 {
            panic!("cursor never reached mousesess");
        }
        app.handle_key(k_down());
        tries += 1;
    }
    app.handle_key(k_tab());
    poll_for_embedded_grid_contains(&app, "$ ", Duration::from_secs(5))
        .expect("embedded prompt");

    // Generate scrollback so there's something for copy-mode to scroll
    // up *into* — without this, scroll-up still enters mode but is a
    // less interesting smoke. Print 30 lines.
    type_str(&mut app, "for i in $(seq 1 30); do echo line-$i; done");
    app.handle_key(k_enter());
    poll_for_embedded_grid_contains(&app, "line-30", Duration::from_secs(5))
        .expect("scrollback should contain line-30");

    // Renderer normally sets the panel rect each frame. In test
    // (no real ratatui draw loop driving paint), set it directly.
    app.embedded_panel_rect.set(Some((40, 3, 60, 24)));

    // Scroll-up at frame coords (50, 10) — clearly inside (40..100, 3..27).
    let scroll = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 50,
        row: 10,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    app.handle_mouse(scroll);

    // Wait for the tmux server to reflect the mode change.
    let in_mode = poll_until(Duration::from_secs(3), || {
        tmux.pane_in_mode("mousesess").unwrap_or(false)
    });
    assert!(
        in_mode,
        "scroll-up via App::handle_mouse should put tmux pane into \
         copy-mode (proves mouse SGR forwarding through the PTY \
         reached the tmux server)"
    );

    // Cleanup
    app.handle_key(k_ctrl_space());
    app.handle_key(k('q'));
    drop(app);
    drop(tmux);
}

/// External kill of the target session while embedded should be
/// detected on the next `tick()` and exit cleanly back to the tree
/// — no panic, no leaked PTY child.
#[test]
fn acceptance_target_session_killed_during_embed() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("doomed", 80, 24).expect("new-session");

    let mut app = App::new();
    app.handle_key(k('p'));
    let mut tries = 0;
    while app.preview_target().as_ref().map(|k| k.name.as_str()) != Some("doomed") {
        if tries > 10 {
            panic!("cursor never reached doomed");
        }
        app.handle_key(k_down());
        tries += 1;
    }
    app.handle_key(k_tab());
    poll_for_embedded_grid_contains(&app, "$ ", Duration::from_secs(5))
        .expect("embedded prompt");
    assert!(app.embedded_active());

    // Kill the session out from under us.
    tmux.kill_session("doomed").expect("kill the session");

    // Drive ticks until App detects the dead child and exits embedded.
    // Tick is non-blocking; we just call it repeatedly.
    let exited = poll_until(Duration::from_secs(3), || {
        app.tick();
        !app.embedded_active()
    });
    assert!(
        exited,
        "App.tick should detect the dead embedded child and exit \
         embedded mode within 3s"
    );
    // Session is genuinely gone.
    assert!(!tmux.has_session("doomed"));

    drop(app);
    drop(tmux);
}

/// Drop App while embedded — the embedded `tmux attach` child must
/// be reaped (no zombie), and the underlying tmux session must
/// survive (we detached, didn't kill).
#[test]
fn acceptance_drop_app_while_embedded_does_not_kill_session() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("survives", 80, 24).expect("new-session");

    let mut app = App::new();
    app.handle_key(k('p'));
    let mut tries = 0;
    while app.preview_target().as_ref().map(|k| k.name.as_str()) != Some("survives") {
        if tries > 10 {
            panic!("cursor never reached survives");
        }
        app.handle_key(k_down());
        tries += 1;
    }
    app.handle_key(k_tab());
    poll_for_embedded_grid_contains(&app, "$ ", Duration::from_secs(5))
        .expect("embedded prompt");
    assert!(app.embedded_active());
    // Sanity: the session is alive immediately before we drop the
    // App — without this, a passing test wouldn't actually be proving
    // that App's cleanup preserved it.
    assert!(
        tmux.has_session("survives"),
        "pre-drop sanity check failed"
    );

    // Drop App while still embedded. EmbeddedTerm::Drop kills the
    // attach client, which makes tmux record a normal client detach
    // — the session itself must remain alive.
    drop(app);

    // Watch the session for a window long enough for any spurious
    // kill cascade to land. If the session disappears at any point,
    // fail loudly. (Codex Phase-10 review: a single post-drop check
    // could pass on a race where the kill arrives after our sample.)
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        assert!(
            tmux.has_session("survives"),
            "tmux session disappeared after ADE drop — embedded was \
             supposed to detach, not kill"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(tmux);
}

// ───────────── duplicate-session acceptance ─────────────

/// Look up `#{pane_start_command}` for a pane — what tmux was originally
/// asked to run. Persists across the process exiting, so we can still
/// see the requested command even when `claude` isn't installed in the
/// test environment and the new session's pane died immediately.
fn pane_start_command(tmux: &IsolatedTmux, session: &str) -> String {
    let target = format!("={}:", session);
    let out = tmux
        .tmux(&[
            "display-message",
            "-p",
            "-t",
            &target,
            "#{pane_start_command}",
        ])
        .output()
        .expect("display-message pane_start_command");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn pane_current_path(tmux: &IsolatedTmux, session: &str) -> String {
    let target = format!("={}:", session);
    let out = tmux
        .tmux(&[
            "display-message",
            "-p",
            "-t",
            &target,
            "#{pane_current_path}",
        ])
        .output()
        .expect("display-message pane_current_path");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// No-Claude branch: duplicate a plain bash session and verify the new
/// session lives in the same cwd as the source, with no startup command.
#[test]
fn acceptance_duplicate_session_no_claude() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("src", 80, 24).expect("new-session");
    // Don't kill sessions when their first command dies — we want to
    // inspect the duplicate session's pane even if its command exits.
    // Must be set after a session exists so the server is kept alive.
    tmux.set_option("remain-on-exit", "on")
        .expect("remain-on-exit on");
    // Let bash settle so pane_current_path is populated.
    let _ = poll_for_capture_contains(&tmux, "src", "$ ", Duration::from_secs(2));

    let src_cwd = pane_current_path(&tmux, "src");
    assert!(!src_cwd.is_empty(), "source pane should have a cwd");

    LocalTmux
        .duplicate_session("src", "dup", false)
        .expect("duplicate (no claude) should succeed");

    assert!(
        tmux.has_session("dup"),
        "duplicated session 'dup' should exist"
    );
    let dup_cwd = pane_current_path(&tmux, "dup");
    assert_eq!(
        dup_cwd, src_cwd,
        "duplicate should land in the source's cwd"
    );
    let cmd = pane_start_command(&tmux, "dup");
    assert!(
        cmd.is_empty() || cmd == "default-shell",
        "no-claude duplicate should have no custom start command (got: {:?})",
        cmd
    );

    drop(tmux);
}

/// Claude branch with a jsonl staged: duplicate should pass
/// `claude --resume <uuid> --fork-session` as the pane's start command.
#[test]
fn acceptance_duplicate_session_claude_with_jsonl() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("src", 80, 24).expect("new-session");
    tmux.set_option("remain-on-exit", "on")
        .expect("remain-on-exit on");
    let _ = poll_for_capture_contains(&tmux, "src", "$ ", Duration::from_secs(2));

    let src_cwd = pane_current_path(&tmux, "src");
    assert!(!src_cwd.is_empty(), "source pane should have a cwd");

    // Stage a fake `.jsonl` under the isolated HOME so the project-dir
    // lookup picks it up. IsolatedTmux::spawn already set HOME to a tmp
    // dir, so this stays out of the user's real ~/.claude.
    let encoded: String = src_cwd
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    let proj_dir = std::path::PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(".claude/projects")
        .join(encoded);
    std::fs::create_dir_all(&proj_dir).expect("mkdir project dir");
    let uuid = "01234567-89ab-cdef-0123-456789abcdef";
    let jsonl = proj_dir.join(format!("{}.jsonl", uuid));
    std::fs::write(&jsonl, b"").expect("write fake jsonl");

    LocalTmux
        .duplicate_session("src", "dup", true)
        .expect("duplicate (claude with jsonl) should succeed");

    assert!(tmux.has_session("dup"), "duplicate session should exist");
    let dup_cwd = pane_current_path(&tmux, "dup");
    assert_eq!(dup_cwd, src_cwd, "duplicate should land in source's cwd");
    // tmux records `pane_start_command` with the original quoting tmux
    // saw it as. We pass the claude command wrapped in `bash -lc '…'`
    // so login-shell PATH init runs. Match by substring on both parts.
    let cmd = pane_start_command(&tmux, "dup");
    let needle = format!("claude --resume {} --fork-session", uuid);
    assert!(
        cmd.contains(&needle),
        "duplicate should launch claude with --resume <uuid> --fork-session — got: {:?}",
        cmd
    );
    assert!(
        cmd.contains("bash -lc"),
        "duplicate must wrap claude in `bash -lc` so PATH from user \
         profile is in effect — got: {:?}",
        cmd
    );

    drop(tmux);
}

/// Claude branch with no jsonl available: should fall back to plain
/// `claude` (no --resume), preserving the cwd.
#[test]
fn acceptance_duplicate_session_claude_no_jsonl() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("src", 80, 24).expect("new-session");
    tmux.set_option("remain-on-exit", "on")
        .expect("remain-on-exit on");
    let _ = poll_for_capture_contains(&tmux, "src", "$ ", Duration::from_secs(2));

    // No project dir staged under HOME — find_latest_session_id returns
    // None, so the fallback path should be exercised.
    LocalTmux
        .duplicate_session("src", "dup", true)
        .expect("duplicate (claude no jsonl) should succeed");

    assert!(tmux.has_session("dup"), "duplicate session should exist");
    let cmd = pane_start_command(&tmux, "dup");
    assert!(
        cmd.contains("claude") && cmd.contains("bash -lc"),
        "no-jsonl claude duplicate should launch plain `claude` via \
         `bash -lc` — got: {:?}",
        cmd
    );

    drop(tmux);
}

/// App-layer correctness: when the refreshed `Session.claude_present`
/// is true (idle Claude — `state` is None because the hook only writes
/// on transitions), pressing y+Enter must still launch `claude` in the
/// new tmux session, not fall back to a plain shell.
///
/// We can't reliably get a real test pane to report `claude` as its
/// `pane_current_command` (macOS reports the basename of whatever process
/// is foreground in the TTY, and tmux invokes scripts via /bin/sh).
/// What we CAN test deterministically: that the App reads
/// `claude_present` (not `claude.is_some()`) when deciding what to pass
/// to the backend. So we mutate `app.tree.sessions[i].claude_present`
/// post-refresh and observe the backend's behavior.
///
/// The detection itself (refresh → claude_present) is unit-tested in
/// `tmux::claude_rollup_tests::idle_claude_sets_present_even_without_status_file`.
#[test]
fn acceptance_duplicate_uses_claude_present_not_claude_state() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("idle-claude", 80, 24)
        .expect("new-session");
    tmux.set_option("remain-on-exit", "on")
        .expect("remain-on-exit");

    let mut app = App::new();
    // Simulate the refresh having seen an idle Claude pane: no chip
    // state, but claude_present is set. This is exactly what
    // map_claude_states emits for a `claude` foreground process with no
    // matching status row.
    let idx = app
        .tree
        .sessions
        .iter()
        .position(|s| s.raw_name == "idle-claude")
        .expect("session in tree");
    app.tree.sessions[idx].claude = None;
    app.tree.sessions[idx].claude_present = true;

    navigate_to_session(&mut app, "idle-claude");
    app.handle_key(k('y'));
    app.handle_key(k_enter());

    assert!(
        tmux.has_session("idle-claude-copy"),
        "duplicate session should exist"
    );
    let cmd = pane_start_command(&tmux, "idle-claude-copy");
    // No jsonl staged → fallback to plain `claude` (wrapped in bash -lc
    // so user PATH applies). The point of this test is that we get
    // `claude`, not an empty start command (which would mean we ignored
    // claude_present and treated it as no-claude).
    assert!(
        cmd.contains("claude") && cmd.contains("bash -lc"),
        "with claude_present=true and no jsonl, duplicate must launch \
         `claude` via bash -lc — got: {:?}",
        cmd
    );

    drop(app);
    drop(tmux);
}

/// Symmetric counter-test: `claude_present = false` and `claude = None`
/// (the plain bash case) must NOT launch claude. Prevents a regression
/// where someone wires the backend off the wrong field.
#[test]
fn acceptance_duplicate_no_claude_no_present_launches_default_shell() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("plain-bash", 80, 24).expect("new-session");
    tmux.set_option("remain-on-exit", "on")
        .expect("remain-on-exit");

    let mut app = App::new();
    let idx = app
        .tree
        .sessions
        .iter()
        .position(|s| s.raw_name == "plain-bash")
        .expect("session in tree");
    app.tree.sessions[idx].claude = None;
    app.tree.sessions[idx].claude_present = false;

    navigate_to_session(&mut app, "plain-bash");
    app.handle_key(k('y'));
    app.handle_key(k_enter());

    assert!(tmux.has_session("plain-bash-copy"));
    let cmd = pane_start_command(&tmux, "plain-bash-copy");
    assert!(
        cmd.is_empty(),
        "no-claude duplicate should have no start command (got: {:?})",
        cmd
    );

    drop(app);
    drop(tmux);
}

/// Duplicate failure when the source session doesn't exist: should
/// return Err, not panic.
#[test]
fn acceptance_duplicate_session_missing_source() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    // No source created.
    let result = LocalTmux.duplicate_session("ghost", "dup", false);
    assert!(
        result.is_err(),
        "duplicate of nonexistent source should fail, got: {:?}",
        result
    );
    drop(tmux);
}

// ───────────── is_session_uuid unit tests ─────────────
// Inlined here (rather than in src/tmux/mod.rs) to keep test scaffolding
// in one place. The validator gates injection-adjacent code (the only
// thing keeping a maliciously-named jsonl from inlining tokens into the
// tmux command string), so the boundary cases are worth pinning down.

#[test]
fn is_session_uuid_accepts_canonical() {
    use crate::tmux::is_session_uuid;
    assert!(is_session_uuid("01234567-89ab-cdef-0123-456789abcdef"));
    assert!(is_session_uuid("ABCDEF01-2345-6789-ABCD-EF0123456789"));
    assert!(is_session_uuid("00000000-0000-0000-0000-000000000000"));
}

#[test]
fn is_session_uuid_rejects_malformed() {
    use crate::tmux::is_session_uuid;
    // Wrong length
    assert!(!is_session_uuid("short"));
    assert!(!is_session_uuid(""));
    assert!(!is_session_uuid("01234567-89ab-cdef-0123-456789abcdef0"));
    // Dash in wrong position
    assert!(!is_session_uuid("0123456-789ab-cdef-0123-456789abcdef0"));
    // Non-hex
    assert!(!is_session_uuid("Z1234567-89ab-cdef-0123-456789abcdef"));
    // Injection attempt: shell metacharacters
    assert!(!is_session_uuid(
        "01234567-89ab-cdef-0123-456789a;rm -rf /"
    ));
    // 36 chars but missing dashes
    assert!(!is_session_uuid(
        "0123456789abcdef0123456789abcdef0123"
    ));
}

// ───────────── duplicate App-layer integration ─────────────
//
// These exercise the full key path: outer KeyEvent → App::handle_key →
// state machine → backend → tmux. They use the same IsolatedTmux harness
// so App::refresh sees only the test sessions.

/// Drive `k_down()` until `App::preview_target` reports the session
/// `wanted`, or panic after 20 tries (defensive limit; current trees
/// are tiny). Mirrors the navigation pattern used in
/// `acceptance_full_embed_lifecycle`.
fn navigate_to_session(app: &mut App, wanted: &str) {
    for _ in 0..20 {
        if app.preview_target().as_ref().map(|k| k.name.as_str()) == Some(wanted) {
            return;
        }
        app.handle_key(k_down());
    }
    panic!(
        "did not reach session {:?} after 20 down-presses; tree: {:?}",
        wanted,
        app.tree.sessions.iter().map(|s| &s.raw_name).collect::<Vec<_>>()
    );
}

#[test]
fn acceptance_duplicate_y_key_enters_state_with_prefilled_buffer() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("src", 80, 24).expect("new-session");

    let mut app = App::new();
    navigate_to_session(&mut app, "src");

    app.handle_key(k('y'));

    match &app.state {
        AppState::DuplicatingSession { source_name, .. } => {
            assert_eq!(source_name, "src");
        }
        other => panic!("expected DuplicatingSession, got {:?}", other),
    }
    assert_eq!(
        app.input_buffer.as_str(),
        "src-copy",
        "default name should be <source>-copy"
    );
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_duplicate_esc_cancels_back_to_tree() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("src", 80, 24).expect("new-session");

    let mut app = App::new();
    navigate_to_session(&mut app, "src");
    app.handle_key(k('y'));
    app.handle_key(k_esc());

    assert!(
        matches!(app.state, AppState::Tree),
        "Esc should return to Tree state, got {:?}",
        app.state
    );
    assert!(
        app.input_buffer.is_empty(),
        "Esc should clear the input buffer"
    );
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_duplicate_enter_creates_session_via_backend() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("src", 80, 24).expect("new-session");
    tmux.set_option("remain-on-exit", "on")
        .expect("remain-on-exit");

    let mut app = App::new();
    navigate_to_session(&mut app, "src");
    app.handle_key(k('y'));
    app.handle_key(k_enter());

    assert!(
        tmux.has_session("src-copy"),
        "Enter on duplicate modal should create the new session"
    );
    assert!(
        matches!(app.state, AppState::Tree),
        "success should return to Tree state"
    );
    assert!(
        app.error_message.is_none(),
        "no error expected on success: {:?}",
        app.error_message
    );
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_duplicate_collision_blocked_at_app_layer() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("src", 80, 24).expect("new-session");
    // Pre-create the target name so the precheck must reject.
    tmux.new_session("src-copy", 80, 24)
        .expect("new src-copy session");

    let mut app = App::new();
    navigate_to_session(&mut app, "src");
    app.handle_key(k('y'));
    app.handle_key(k_enter());

    let err = app
        .error_message
        .as_ref()
        .expect("collision should set error_message");
    assert!(
        err.contains("already exists"),
        "collision banner should mention the conflict; got: {:?}",
        err
    );
    assert!(
        matches!(app.state, AppState::Tree),
        "collision path should land back on Tree"
    );
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_duplicate_action_cycle_session_row_includes_duplicate() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("src", 80, 24).expect("new-session");

    let mut app = App::new();
    navigate_to_session(&mut app, "src");

    // Starts at Enter.
    assert_eq!(app.selected_action, SessionAction::Enter);
    app.handle_key(k('l'));
    assert_eq!(app.selected_action, SessionAction::Rename);
    app.handle_key(k('l'));
    assert_eq!(
        app.selected_action,
        SessionAction::Duplicate,
        "cycle on a session row should reach Duplicate"
    );
    app.handle_key(k('l'));
    assert_eq!(app.selected_action, SessionAction::Delete);
    // Delete is the right endpoint — further `l` stays there.
    app.handle_key(k('l'));
    assert_eq!(app.selected_action, SessionAction::Delete);
    // Reverse: Delete → Duplicate → Rename → Enter (sticky at Enter).
    app.handle_key(k('h'));
    assert_eq!(app.selected_action, SessionAction::Duplicate);
    app.handle_key(k('h'));
    assert_eq!(app.selected_action, SessionAction::Rename);
    app.handle_key(k('h'));
    assert_eq!(app.selected_action, SessionAction::Enter);
    app.handle_key(k('h'));
    assert_eq!(app.selected_action, SessionAction::Enter);
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_duplicate_action_cycle_folder_row_skips_duplicate() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    // Two sessions sharing a `proj/` prefix promote `proj` to a folder
    // row in the tree.
    tmux.new_session("proj/a", 80, 24)
        .expect("new-session proj/a");
    tmux.new_session("proj/b", 80, 24)
        .expect("new-session proj/b");

    let mut app = App::new();
    // Folder rows live at the start of the visible_rows list. Walk up
    // until we hit the folder.
    for _ in 0..20 {
        if matches!(app.current_row(), Some(crate::model::Row::Folder(_))) {
            break;
        }
        app.handle_key(key_press(KeyCode::Up, KeyModifiers::NONE));
    }
    assert!(
        matches!(app.current_row(), Some(crate::model::Row::Folder(_))),
        "expected to land on the proj/ folder row"
    );

    assert_eq!(app.selected_action, SessionAction::Enter);
    app.handle_key(k('l'));
    assert_eq!(app.selected_action, SessionAction::Rename);
    app.handle_key(k('l'));
    assert_eq!(
        app.selected_action,
        SessionAction::Delete,
        "folder rows should NOT have a Duplicate slot — l from Rename \
         goes directly to Delete"
    );
    // Reverse from Delete: skip Duplicate, back to Rename.
    app.handle_key(k('h'));
    assert_eq!(app.selected_action, SessionAction::Rename);
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_duplicate_y_on_folder_row_is_noop() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("proj/a", 80, 24).expect("new-session");
    tmux.new_session("proj/b", 80, 24).expect("new-session");

    let mut app = App::new();
    for _ in 0..20 {
        if matches!(app.current_row(), Some(crate::model::Row::Folder(_))) {
            break;
        }
        app.handle_key(key_press(KeyCode::Up, KeyModifiers::NONE));
    }
    assert!(matches!(
        app.current_row(),
        Some(crate::model::Row::Folder(_))
    ));

    app.handle_key(k('y'));
    assert!(
        matches!(app.state, AppState::Tree),
        "y on a folder row must NOT enter DuplicatingSession state; \
         got {:?}",
        app.state
    );
    drop(app);
    drop(tmux);
}

// ───────────── remote shell-script substitution (string-level) ─────────────
//
// Can't run an SSH duplicate against an isolated remote, so we pin the
// shell script that the remote backend assembles. Reading the constructed
// command is the next-best thing to running it — it locks in the
// shell-quoting decisions we made (single-quoted SRC/NEW substitution,
// trailing colon on SRC for pane targeting, exec tmux fallthrough).

#[test]
fn remote_duplicate_script_substitution_pins_format() {
    use crate::hosts::{Host, HostKind};
    use crate::tmux::remote::RemoteTmux;
    // We're not actually going to invoke ssh — just construct the
    // command via the same path the real code uses. The unit test calls
    // a private helper to keep things stable; if we ever refactor the
    // format!, the test pins what the script SHOULD look like.
    let host = Host {
        name: "test".into(),
        target: "user@example".into(),
        kind: HostKind::Ssh,
        ssh_args: vec![],
    };
    let r = RemoteTmux { host };
    let cmd = r.build_duplicate_cmd("src", "dup", true);
    // Trailing colon on SRC — critical for pane-scope format vars.
    assert!(cmd.contains("SRC='=src:'"), "SRC must end with `:`: {}", cmd);
    assert!(cmd.contains("NEW='dup'"), "NEW substitution missing: {}", cmd);
    assert!(cmd.contains("CLAUDE=1"), "CLAUDE flag should be 1: {}", cmd);
    assert!(
        cmd.contains("--fork-session"),
        "script must include --fork-session: {}",
        cmd
    );
    // UUID case glob: hex chars only, no `.` or `*` wildcards.
    assert!(
        cmd.contains("[0-9A-Fa-f]"),
        "UUID glob must use hex character class: {}",
        cmd
    );
    // The session-id selection must ITERATE files (not just take the
    // newest), so it can skip a non-UUID newer entry and reach an older
    // valid UUID. The presence of `for f in $(ls -1t` + a matching
    // `break` is the structural signal we pin on. Regressing to
    // `ls -1t … | head -n 1` (the original buggy approach) would not
    // contain `break` or `for f`.
    assert!(
        cmd.contains("for f in $(ls -1t"),
        "must iterate jsonl files newest-first, not pick a single newest: {}",
        cmd
    );
    assert!(
        cmd.contains("break"),
        "iteration must break on the first valid UUID match: {}",
        cmd
    );
    // IFS must be set to a literal newline before the for-loop so paths
    // with spaces (e.g. `~/Documents/my project/`) don't word-split. The
    // newline char is embedded in the Rust string literal via `\n`, which
    // becomes a real LF in the assembled shell command.
    assert!(
        cmd.contains("IFS='\n'"),
        "must set IFS to newline before iterating ls output (otherwise \
         paths with spaces silently break the UUID match): {}",
        cmd
    );
    assert!(
        cmd.contains("__OLDIFS"),
        "must save and restore IFS around the loop: {}",
        cmd
    );
    // Branches build `INNER` (bare command) then wrap in `bash -lc`.
    assert!(
        cmd.contains("INNER=\"claude --resume $SID --fork-session\""),
        "SID branch must set INNER to the resume command: {}",
        cmd
    );
    assert!(
        cmd.contains("INNER=\"claude\""),
        "no-SID-with-CLAUDE branch must set INNER to plain claude: {}",
        cmd
    );
    // The wrapping is what fixes the PATH issue — assert it.
    assert!(
        cmd.contains("CMD=\"bash -lc '$INNER'\""),
        "claude command must be wrapped in bash -lc so login-shell PATH \
         applies (otherwise `claude: command not found` when binary lives \
         in a user-managed bin dir like nvm/asdf): {}",
        cmd
    );
    // The no-CLAUDE branch is the only one that runs new-session with no
    // command (defaults to the default shell, which is fine).
    assert!(
        cmd.contains("tmux new-session -d -s \"$NEW\" -c \"$CWD\";"),
        "no-claude fallback (no command) missing: {}",
        cmd
    );
}

/// Run the remote duplicate script under `/bin/sh` against a fake
/// project directory whose path contains spaces. Without the IFS=newline
/// guard, the inner `for f in $(ls …)` word-splits the paths and the
/// UUID `case` rejects every fragment, silently falling through to the
/// plain-claude branch. We capture stdout — the script normally calls
/// `exec tmux new-session …` at the end; here we shadow `tmux` with an
/// `echo` stub so the test sees exactly which branch the script took.
#[test]
fn remote_duplicate_script_handles_path_with_spaces() {
    use crate::hosts::{Host, HostKind};
    use crate::tmux::remote::RemoteTmux;
    use std::process::Command;

    // Stage a project dir with a space in its path. The encoded form
    // (`/` → `-`) preserves the space.
    let home = std::env::temp_dir().join(format!(
        "ade-remote-space-{}-{}",
        std::process::id(),
        std::time::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).expect("mk home");
    let fake_cwd = "/Users/foo/Has Space/dir";
    let encoded: String = fake_cwd
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    let proj = home.join(".claude/projects").join(&encoded);
    std::fs::create_dir_all(&proj).expect("mk proj");
    let uuid = "deadbeef-1234-5678-9abc-def012345678";
    std::fs::write(proj.join(format!("{}.jsonl", uuid)), b"")
        .expect("touch jsonl");

    // Build a stub `tmux` and `claude` on PATH that echo their args.
    // The script first calls `tmux display-message …` to read CWD, so
    // our stub needs to handle that case too.
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).expect("mk bin");
    std::fs::write(
        bin.join("tmux"),
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"display-message\" ]; then echo '{}'; exit 0; fi\n\
             echo \"EXEC tmux $*\"; exit 0\n",
            fake_cwd
        ),
    )
    .expect("write tmux stub");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(bin.join("tmux"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");

    let host = Host {
        name: "x".into(),
        target: "u@x".into(),
        kind: HostKind::Ssh,
        ssh_args: vec![],
    };
    let r = RemoteTmux { host };
    // CLAUDE=1 so the SID-discovery loop runs.
    let script = r.build_duplicate_cmd("src", "dup", true);

    // Pin the script's working directory to a known-empty path so
    // word-split fragments (e.g. "Has", "Space", "dir/uuid.jsonl") can
    // never accidentally resolve to real files via relative paths — that
    // would let the buggy IFS=default implementation false-pass.
    let empty_cwd = home.join("empty-cwd");
    std::fs::create_dir_all(&empty_cwd).expect("mk empty cwd");
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .env("HOME", &home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .current_dir(&empty_cwd)
        .output()
        .expect("run script");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The script's final branch is `exec tmux new-session …`. Under
    // exec, the script process is replaced by tmux; our stub then
    // echoes `EXEC tmux new-session …` to stdout. The expected line
    // includes `claude --resume <uuid> --fork-session` IFF the
    // discovery loop succeeded.
    let expected = format!(
        "claude --resume {} --fork-session",
        uuid
    );
    assert!(
        stdout.contains(&expected),
        "script should have found UUID jsonl despite the space in CWD; \
         stdout: {:?}\nstderr: {:?}\nscript: {}",
        stdout,
        stderr,
        script
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn remote_duplicate_script_claude_false_produces_no_session_id_branch() {
    use crate::hosts::{Host, HostKind};
    use crate::tmux::remote::RemoteTmux;
    let host = Host {
        name: "t".into(),
        target: "u@h".into(),
        kind: HostKind::Ssh,
        ssh_args: vec![],
    };
    let r = RemoteTmux { host };
    let cmd = r.build_duplicate_cmd("src", "dup", false);
    assert!(cmd.contains("CLAUDE=0"), "CLAUDE flag should be 0: {}", cmd);
    // With CLAUDE=0, INNER stays empty and we hit the `else` branch
    // that calls `tmux new-session` without a command (default shell).
    assert!(
        cmd.contains("tmux new-session -d -s \"$NEW\" -c \"$CWD\""),
        "no-claude fallback (default shell) missing: {}",
        cmd
    );
}

#[test]
fn remote_duplicate_rejects_unsafe_names() {
    use crate::hosts::{Host, HostKind};
    use crate::tmux::remote::RemoteTmux;
    use crate::tmux::TmuxBackend;
    let host = Host {
        name: "x".into(),
        target: "user@x".into(),
        kind: HostKind::Ssh,
        ssh_args: vec![],
    };
    let r = RemoteTmux { host };
    // Spaces aren't allowed; the existing shell_safe filter must reject.
    assert!(r.duplicate_session("a b", "dup", false).is_err());
    assert!(r.duplicate_session("src", "d;p", false).is_err());
    assert!(r.duplicate_session("", "dup", false).is_err());
}

