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

use crate::app::{App, AppAction, AppState, CreateField, SessionAction};
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
    assert_eq!(
        app.selected_action,
        SessionAction::NewSession,
        "folder rows expose a NewSession slot after Toggle"
    );
    app.handle_key(k('l'));
    assert_eq!(app.selected_action, SessionAction::Rename);
    app.handle_key(k('l'));
    assert_eq!(
        app.selected_action,
        SessionAction::Delete,
        "folder rows should NOT have a Duplicate slot — l from Rename \
         goes directly to Delete"
    );
    // Reverse from Delete: skip Duplicate, back to Rename → NewSession → Enter.
    app.handle_key(k('h'));
    assert_eq!(app.selected_action, SessionAction::Rename);
    app.handle_key(k('h'));
    assert_eq!(app.selected_action, SessionAction::NewSession);
    app.handle_key(k('h'));
    assert_eq!(app.selected_action, SessionAction::Enter);
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

// ───────────── folder NewSession action (chip + `n` prefill) ─────────────

fn navigate_to_folder(app: &mut App) {
    for _ in 0..20 {
        if matches!(app.current_row(), Some(crate::model::Row::Folder(_))) {
            return;
        }
        app.handle_key(key_press(KeyCode::Up, KeyModifiers::NONE));
    }
    panic!("did not reach a Folder row after 20 up-presses");
}

#[test]
fn acceptance_folder_new_chip_opens_modal_with_prefix() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("proj/a", 80, 24).expect("new-session proj/a");
    tmux.new_session("proj/b", 80, 24).expect("new-session proj/b");

    let mut app = App::new();
    navigate_to_folder(&mut app);

    // l once → NewSession chip active.
    app.handle_key(k('l'));
    assert_eq!(app.selected_action, SessionAction::NewSession);

    app.handle_key(k_enter());
    match &app.state {
        AppState::CreatingSession(form) => {
            assert_eq!(
                form.prefix.as_str(),
                "proj",
                "modal prefix should be folder.prefix"
            );
            assert!(form.name.is_empty(), "name should start empty");
            assert_eq!(
                form.focus,
                CreateField::Name,
                "focus should jump past the pre-filled Prefix"
            );
        }
        other => panic!("expected CreatingSession state, got {:?}", other),
    }
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_n_on_folder_prefills_prefix() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("proj/a", 80, 24).expect("new-session proj/a");
    tmux.new_session("proj/b", 80, 24).expect("new-session proj/b");

    let mut app = App::new();
    navigate_to_folder(&mut app);

    app.handle_key(k('n'));
    match &app.state {
        AppState::CreatingSession(form) => {
            assert_eq!(form.prefix.as_str(), "proj");
            assert_eq!(form.focus, CreateField::Name);
        }
        other => panic!("expected CreatingSession, got {:?}", other),
    }
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_n_on_session_in_folder_inherits_prefix() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("proj/alpha", 80, 24)
        .expect("new-session proj/alpha");
    tmux.new_session("proj/beta", 80, 24)
        .expect("new-session proj/beta");

    let mut app = App::new();
    navigate_to_session(&mut app, "proj/alpha");

    app.handle_key(k('n'));
    match &app.state {
        AppState::CreatingSession(form) => {
            assert_eq!(
                form.prefix.as_str(),
                "proj",
                "session inside a folder should inherit the parent prefix"
            );
            assert_eq!(form.focus, CreateField::Name);
        }
        other => panic!("expected CreatingSession, got {:?}", other),
    }
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_n_on_toplevel_session_falls_back_to_default() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    // "loose" has no `/` → Session.prefix is None, so the `n` handler
    // must fall back to CreateForm::new() (cwd-guessed prefix).
    tmux.new_session("loose", 80, 24).expect("new-session loose");

    let mut app = App::new();
    navigate_to_session(&mut app, "loose");

    app.handle_key(k('n'));
    match &app.state {
        AppState::CreatingSession(_) => {
            // We can't assert the exact prefix (cwd-dependent), but the
            // fallback path is taken iff no panic — the prior implementation
            // would have unconditionally called CreateForm::new() too.
        }
        other => panic!("expected CreatingSession, got {:?}", other),
    }
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_stale_new_session_on_session_row_attaches_instead_of_deadkey() {
    // Regression: if a folder is demoted to a plain session by a refresh
    // (e.g. a sibling was renamed out so only one prefixed session remains —
    // though in this constructed scenario we just stash the action manually)
    // while its NewSession chip is the selected action, pressing Enter on
    // the now-session row should attach rather than silently no-op.
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("solo", 80, 24).expect("new-session solo");

    let mut app = App::new();
    navigate_to_session(&mut app, "solo");

    // Stash the stale chip state.
    app.selected_action = SessionAction::NewSession;
    app.handle_key(k_enter());

    match &app.action {
        AppAction::AttachSession { name, .. } => {
            assert_eq!(name, "solo", "Enter on stale NewSession should attach");
        }
        other => panic!("expected AttachSession, got {:?}", other),
    }
    // Action should also normalize back to Enter for subsequent keypresses.
    assert_eq!(app.selected_action, SessionAction::Enter);
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_create_modal_tab_cycles_machines_then_fields() {
    // Tab in the create-session modal should step through every option in
    // order — each available machine, then Prefix, then Name — so a user
    // can navigate the whole modal without arrow keys.
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();

    let mut app = App::new();
    // Inject two fake remotes so available_machines() returns three entries.
    app.config.hosts = vec![
        crate::hosts::Host {
            name: "alpha".to_string(),
            kind: crate::hosts::HostKind::Ssh,
            target: "alpha.example".to_string(),
            ssh_args: vec![],
        },
        crate::hosts::Host {
            name: "beta".to_string(),
            kind: crate::hosts::HostKind::Ssh,
            target: "beta.example".to_string(),
            ssh_args: vec![],
        },
    ];

    // Open the modal directly so the test doesn't depend on tree state.
    app.state = AppState::CreatingSession(crate::app::CreateForm::new());
    // Reset to a known starting point: Machine field, Local selected.
    if let AppState::CreatingSession(ref mut form) = app.state {
        form.focus = CreateField::Machine;
        form.machine = crate::model::Machine::Local;
    }

    let tab = key_press(KeyCode::Tab, KeyModifiers::NONE);

    // Tab 1: Local → alpha (still on Machine).
    app.handle_key(tab);
    match &app.state {
        AppState::CreatingSession(f) => {
            assert_eq!(f.focus, CreateField::Machine);
            assert_eq!(f.machine.label(), "alpha");
        }
        other => panic!("expected CreatingSession, got {:?}", other),
    }

    // Tab 2: alpha → beta (still on Machine).
    app.handle_key(tab);
    if let AppState::CreatingSession(ref f) = app.state {
        assert_eq!(f.focus, CreateField::Machine);
        assert_eq!(f.machine.label(), "beta");
    }

    // Tab 3: at last machine → focus moves to Prefix (machine stays beta).
    app.handle_key(tab);
    if let AppState::CreatingSession(ref f) = app.state {
        assert_eq!(f.focus, CreateField::Prefix);
        assert_eq!(f.machine.label(), "beta");
    }

    // Tab 4: Prefix → Name.
    app.handle_key(tab);
    if let AppState::CreatingSession(ref f) = app.state {
        assert_eq!(f.focus, CreateField::Name);
    }

    // Tab 5: Name → Machine, resetting to the first machine.
    app.handle_key(tab);
    if let AppState::CreatingSession(ref f) = app.state {
        assert_eq!(f.focus, CreateField::Machine);
        assert_eq!(f.machine.label(), "Local");
    }

    // Shift+Tab from Machine[0] wraps backward to Name.
    let back_tab = key_press(KeyCode::BackTab, KeyModifiers::NONE);
    app.handle_key(back_tab);
    if let AppState::CreatingSession(ref f) = app.state {
        assert_eq!(f.focus, CreateField::Name);
    }

    // Shift+Tab from Name → Prefix.
    app.handle_key(back_tab);
    if let AppState::CreatingSession(ref f) = app.state {
        assert_eq!(f.focus, CreateField::Prefix);
    }

    // Shift+Tab from Prefix → Machine, snapping to the last machine.
    app.handle_key(back_tab);
    if let AppState::CreatingSession(ref f) = app.state {
        assert_eq!(f.focus, CreateField::Machine);
        assert_eq!(f.machine.label(), "beta");
    }

    // Shift+Tab from Machine[last] retreats to the previous machine.
    app.handle_key(back_tab);
    if let AppState::CreatingSession(ref f) = app.state {
        assert_eq!(f.focus, CreateField::Machine);
        assert_eq!(f.machine.label(), "alpha");
    }

    // Stale machine: simulate a host being deleted while the modal is open.
    // Tab should normalize back to Local before stepping (so the next stop
    // is alpha, the first remote), rather than skipping Local.
    if let AppState::CreatingSession(ref mut f) = app.state {
        f.focus = CreateField::Machine;
        f.machine = crate::model::Machine::Remote("ghost".to_string());
    }
    app.handle_key(tab);
    if let AppState::CreatingSession(ref f) = app.state {
        assert_eq!(f.focus, CreateField::Machine);
        assert_eq!(
            f.machine.label(),
            "alpha",
            "stale machine should normalize to Local then advance to first remote"
        );
    }

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

// ───────────── folder delete / dissolve (3-way confirm) ─────────────
//
// Two sessions sharing a `proj/` prefix form a folder. Pressing `d` on
// the folder row should open a confirm modal whose primary action kills
// the sessions and whose alternate (`s`) renames them to strip the
// prefix. Cancel paths (Esc, n) must leave tmux untouched.

fn navigate_to_first_folder(app: &mut App) {
    for _ in 0..20 {
        if matches!(app.current_row(), Some(crate::model::Row::Folder(_))) {
            return;
        }
        app.handle_key(key_press(KeyCode::Up, KeyModifiers::NONE));
    }
    panic!("did not reach a folder row after 20 up-presses");
}

#[test]
fn acceptance_folder_d_opens_delete_with_dissolve_alternate() {
    use crate::app::{ConfirmAlternate, PendingAction, PendingConfirm};

    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("proj/a", 80, 24).expect("new proj/a");
    tmux.new_session("proj/b", 80, 24).expect("new proj/b");

    let mut app = App::new();
    navigate_to_first_folder(&mut app);
    app.handle_key(k('d'));

    match &app.state {
        AppState::Confirming(PendingConfirm {
            title,
            action: PendingAction::DeleteFolder { prefix, targets },
            alternate:
                Some(ConfirmAlternate {
                    key: alt_key,
                    action: PendingAction::DissolveFolder {
                        prefix: alt_prefix,
                        ..
                    },
                    ..
                }),
            ..
        }) => {
            assert_eq!(title, "Delete folder");
            assert_eq!(prefix, "proj");
            assert_eq!(targets.len(), 2);
            let names: Vec<&str> = targets.iter().map(|(_, n)| n.as_str()).collect();
            assert!(names.contains(&"proj/a") && names.contains(&"proj/b"));
            assert_eq!(*alt_key, 's');
            assert_eq!(alt_prefix, "proj");
        }
        other => panic!(
            "expected Confirming(DeleteFolder, alt=DissolveFolder), got {:?}",
            other
        ),
    }
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_folder_d_then_enter_kills_all_sessions() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("proj/a", 80, 24).expect("new proj/a");
    tmux.new_session("proj/b", 80, 24).expect("new proj/b");

    let mut app = App::new();
    navigate_to_first_folder(&mut app);
    app.handle_key(k('d'));
    app.handle_key(k_enter());

    assert!(
        matches!(app.state, AppState::Tree),
        "should be back in Tree, got {:?}",
        app.state
    );
    assert!(!tmux.has_session("proj/a"), "proj/a should be killed");
    assert!(!tmux.has_session("proj/b"), "proj/b should be killed");
    assert!(
        app.error_message.is_none(),
        "no error expected: {:?}",
        app.error_message
    );
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_folder_d_then_s_dissolves_and_keeps_sessions() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("proj/a", 80, 24).expect("new proj/a");
    tmux.new_session("proj/b", 80, 24).expect("new proj/b");

    let mut app = App::new();
    navigate_to_first_folder(&mut app);
    app.handle_key(k('d'));
    app.handle_key(k('s'));

    assert!(matches!(app.state, AppState::Tree));
    assert!(!tmux.has_session("proj/a"), "proj/a should be renamed");
    assert!(!tmux.has_session("proj/b"), "proj/b should be renamed");
    assert!(tmux.has_session("a"), "renamed `a` should be alive");
    assert!(tmux.has_session("b"), "renamed `b` should be alive");
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_folder_d_then_esc_leaves_tmux_untouched() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("proj/a", 80, 24).expect("new proj/a");
    tmux.new_session("proj/b", 80, 24).expect("new proj/b");

    let mut app = App::new();
    navigate_to_first_folder(&mut app);
    app.handle_key(k('d'));
    app.handle_key(k_esc());

    assert!(matches!(app.state, AppState::Tree));
    assert!(tmux.has_session("proj/a"));
    assert!(tmux.has_session("proj/b"));
    drop(app);
    drop(tmux);
}

// ───────────── kanban board ─────────────

use crate::kanban;
use crate::model::Machine;
use crate::state::{PlacementEntry, State};

fn k_upper(c: char) -> KeyEvent {
    key_press(KeyCode::Char(c), KeyModifiers::SHIFT)
}

/// Column indices of the default kanban config, resolved by id so the
/// tests don't hardcode positions.
fn default_col(app: &App, id: &str) -> usize {
    app.kanban_config
        .idx_of_id(id)
        .unwrap_or_else(|| panic!("default config should have column '{}'", id))
}

fn board_of(app: &App) -> Vec<Vec<usize>> {
    kanban::build_board(&app.kanban_config, &app.tree.sessions, &app.kanban_placements)
}

/// Names of the sessions in the given board column.
fn col_names(app: &App, board: &[Vec<usize>], col: usize) -> Vec<String> {
    board[col]
        .iter()
        .filter_map(|&si| app.tree.session(si))
        .map(|s| s.raw_name.clone())
        .collect()
}

/// Seed a manual placement into the isolated state.toml BEFORE App::new
/// (which loads placements exactly once at startup).
fn seed_placement(session: &str, column: &str) {
    let mut state = State::load();
    state.kanban.placements.push(PlacementEntry {
        host: "local".to_string(),
        session: session.to_string(),
        column: column.to_string(),
    });
    state.save().expect("seed placement save");
}

/// Launch a fake `claude` (a tiny compiled sleeper — copying or
/// symlinking `/bin/sleep` doesn't survive macOS: a copied platform
/// binary is killed by AMFI, and `pane_current_command` resolves a
/// symlink to the real image name) in the session's pane, wait until
/// tmux reports it as the foreground command, then write a hook-style
/// status file for that pane. Returns the pane id. `cc` is guaranteed
/// wherever `cargo test` runs — rustc needs it to link.
fn fake_claude(tmux: &IsolatedTmux, session: &str, state_json: &str) -> String {
    let home = std::path::PathBuf::from(std::env::var("HOME").expect("HOME set"));
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).expect("mk fake bin dir");
    let fake = bin.join("claude");
    if !fake.exists() {
        let src = home.join("fake_claude.c");
        std::fs::write(&src, "#include <unistd.h>\nint main(void){sleep(300);return 0;}\n")
            .expect("write fake claude source");
        let out = std::process::Command::new("cc")
            .arg("-o")
            .arg(&fake)
            .arg(&src)
            .output()
            .expect("run cc");
        assert!(
            out.status.success(),
            "cc failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    tmux.send_keys(session, "PATH=$HOME/bin:$PATH claude\r")
        .expect("launch fake claude");
    let ok = poll_until(Duration::from_secs(3), || {
        tmux.pane_current_command(session)
            .map(|c| c == "claude")
            .unwrap_or(false)
    });
    assert!(ok, "fake claude never became the foreground command");

    let pane_id = tmux.pane_id(session).expect("pane id");
    write_status(&pane_id, state_json);
    pane_id
}

/// Write `~/.cache/ade/claude-status/<pane_id>.json` the way the hooks do.
fn write_status(pane_id: &str, state_json: &str) {
    let home = std::path::PathBuf::from(std::env::var("HOME").expect("HOME set"));
    let dir = home.join(".cache").join("ade").join("claude-status");
    std::fs::create_dir_all(&dir).expect("mk status dir");
    std::fs::write(dir.join(format!("{}.json", pane_id)), state_json)
        .expect("write status file");
}

#[test]
fn acceptance_kanban_enter_exit_and_default_column() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("kana", 80, 24).expect("new kana");
    tmux.new_session("kanb", 80, 24).expect("new kanb");

    let mut app = App::new();
    app.handle_key(k_upper('K'));
    assert!(
        matches!(app.state, AppState::Kanban(_)),
        "K should enter the kanban board, got {:?}",
        app.state
    );

    // No Claude anywhere, no placements → both sessions sit in the
    // auto-awaiting column; every other column is empty.
    let board = board_of(&app);
    let awaiting = default_col(&app, "awaiting");
    assert_eq!(col_names(&app, &board, awaiting), vec!["kana", "kanb"]);
    for (i, col) in board.iter().enumerate() {
        if i != awaiting {
            assert!(col.is_empty(), "column {} should be empty", i);
        }
    }

    app.handle_key(k_esc());
    assert!(matches!(app.state, AppState::Tree), "Esc returns to tree");
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_kanban_move_card_persists_and_survives_restart() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("kanmove", 80, 24).expect("new kanmove");

    let mut app = App::new();
    app.handle_key(k_upper('K'));
    // Board opens on column 0 (Idle, empty). Step right to awaiting,
    // where the session sits.
    app.handle_key(k('l'));
    // One move right: the auto-active column is skipped as a target, so
    // the card lands directly in Done.
    app.handle_key(k_upper('L'));

    let done = default_col(&app, "done");
    assert_eq!(
        app.kanban_placements
            .get(&(Machine::Local, "kanmove".to_string()))
            .map(String::as_str),
        Some("done"),
        "L from awaiting should place the card in done (skipping active)"
    );
    let board = board_of(&app);
    assert_eq!(col_names(&app, &board, done), vec!["kanmove"]);
    // Focus followed the card.
    let (fcol, fcard) = app.resolve_kanban_focus(&board);
    assert_eq!((fcol, fcard), (done, 0), "cursor should follow the moved card");

    // Persisted?
    let on_disk = State::load();
    assert_eq!(
        on_disk.kanban.placements,
        vec![PlacementEntry {
            host: "local".to_string(),
            session: "kanmove".to_string(),
            column: "done".to_string(),
        }]
    );

    // Survives a full app restart.
    drop(app);
    let app2 = App::new();
    let board2 = board_of(&app2);
    assert_eq!(
        col_names(&app2, &board2, default_col(&app2, "done")),
        vec!["kanmove"],
        "placement should survive App restart via state.toml"
    );

    // Moving back left into awaiting removes the placement entirely
    // ("back to default" is expressible).
    let mut app2 = app2;
    app2.handle_key(k_upper('K'));
    app2.handle_key(k('l')); // idle → awaiting (empty now)... 
    // Navigate focus onto the done column instead of assuming: resolve by
    // stepping right until the focused column is `done`.
    for _ in 0..app2.kanban_config.columns.len() {
        let b = board_of(&app2);
        let (c, _) = app2.resolve_kanban_focus(&b);
        if c == default_col(&app2, "done") {
            break;
        }
        app2.handle_key(k('l'));
    }
    app2.handle_key(k_upper('H'));
    assert!(
        app2.kanban_placements.is_empty(),
        "moving into auto-awaiting should remove the placement"
    );
    assert!(State::load().kanban.placements.is_empty());
    drop(app2);
    drop(tmux);
}

#[cfg(unix)]
#[test]
fn acceptance_kanban_working_forces_active_clears_manual_and_pins_card() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("kanwork", 80, 24).expect("new kanwork");

    // Fake Claude working in the pane + a pre-seeded manual Done
    // placement — the Working precedence must win and CLEAR it.
    let pane_id = fake_claude(&tmux, "kanwork", r#"{"state":"working"}"#);
    seed_placement("kanwork", "done");

    let mut app = App::new(); // refresh + reconcile run inside
    let s = app
        .tree
        .sessions
        .iter()
        .find(|s| s.raw_name == "kanwork")
        .expect("kanwork in tree");
    assert_eq!(
        s.claude,
        Some(crate::claude_status::ClaudeState::Working),
        "fake claude + working status file should read as Working"
    );

    let board = board_of(&app);
    let active = default_col(&app, "active");
    assert_eq!(
        col_names(&app, &board, active),
        vec!["kanwork"],
        "working session belongs to the auto-active column"
    );
    assert!(
        app.kanban_placements.is_empty(),
        "Working must clear the manual Done placement"
    );
    assert!(
        State::load().kanban.placements.is_empty(),
        "the cleared placement must be persisted"
    );

    // A Working card is pinned: moving it out of Active is refused loudly.
    app.handle_key(k_upper('K'));
    for _ in 0..app.kanban_config.columns.len() {
        let b = board_of(&app);
        let (c, _) = app.resolve_kanban_focus(&b);
        if c == active {
            break;
        }
        app.handle_key(k('l'));
    }
    app.handle_key(k_upper('L'));
    assert!(
        app.kanban_placements.is_empty(),
        "blocked move must not create a placement"
    );
    assert!(
        app.error_message.is_some(),
        "blocked move should explain itself in the footer"
    );

    // Claude stops (hook writes idle): the card falls to Awaiting human.
    write_status(&pane_id, r#"{"state":"idle"}"#);
    app.refresh();
    let board = board_of(&app);
    assert_eq!(
        col_names(&app, &board, default_col(&app, "awaiting")),
        vec!["kanwork"],
        "after Working ends the card falls to auto-awaiting"
    );
    assert!(board[active].is_empty());
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_kanban_rename_migrates_placement() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("kanren", 80, 24).expect("new kanren");
    seed_placement("kanren", "verified");

    let mut app = App::new();
    // Cursor starts on the only session row; rename via the tree flow.
    app.handle_key(k_upper('R'));
    assert!(
        matches!(app.state, AppState::RenamingSession { .. }),
        "R should open the rename modal, got {:?}",
        app.state
    );
    // input_buffer is prefilled with "kanren"; append a suffix.
    type_str(&mut app, "2");
    app.handle_key(k_enter());

    assert!(tmux.has_session("kanren2"), "session should be renamed");
    assert_eq!(
        app.kanban_placements
            .get(&(Machine::Local, "kanren2".to_string()))
            .map(String::as_str),
        Some("verified"),
        "placement should migrate to the new session name"
    );
    let on_disk = State::load();
    assert_eq!(on_disk.kanban.placements.len(), 1);
    assert_eq!(on_disk.kanban.placements[0].session, "kanren2");
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_kanban_kill_prunes_placement() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("kankill", 80, 24).expect("new kankill");
    tmux.new_session("kankeep", 80, 24).expect("new kankeep");
    seed_placement("kankill", "done");
    seed_placement("kankeep", "done");

    let mut app = App::new();
    // Navigate the cursor onto kankill (sessions sort alphabetically:
    // kankeep, kankill — so one Down from the top).
    let mut steps = 0;
    loop {
        if let Some(crate::model::Row::Session(idx)) = app.current_row() {
            if app.tree.session(idx).map(|s| s.raw_name.as_str()) == Some("kankill") {
                break;
            }
        }
        assert!(steps < 10, "never found kankill row");
        app.handle_key(k_down());
        steps += 1;
    }
    app.handle_key(k('d'));
    app.handle_key(k_enter());

    assert!(!tmux.has_session("kankill"));
    assert!(
        !app.kanban_placements
            .contains_key(&(Machine::Local, "kankill".to_string())),
        "killed session's placement should be dropped"
    );
    assert_eq!(
        app.kanban_placements
            .get(&(Machine::Local, "kankeep".to_string()))
            .map(String::as_str),
        Some("done"),
        "surviving session's placement must be untouched"
    );
    drop(app);
    drop(tmux);
}

#[test]
fn acceptance_kanban_unobserved_machine_placements_survive_refresh() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("kanlocal", 80, 24).expect("new kanlocal");

    // A host that can't be reached (BatchMode ssh to an invalid target
    // fails fast). Its machine is never in RefreshResult::observed, so
    // its placements must never be pruned — even though its session
    // obviously isn't in the tree.
    let mut hosts = crate::hosts::Config::default();
    hosts
        .upsert(
            crate::hosts::Host {
                name: "ghost".to_string(),
                kind: crate::hosts::HostKind::Ssh,
                target: "invalid.host.ade.test".to_string(),
                ssh_args: vec![],
            },
            None,
        )
        .expect("add ghost host");
    hosts.save().expect("save hosts.toml");

    let mut state = State::load();
    state.kanban.placements.push(PlacementEntry {
        host: "ghost".to_string(),
        session: "far/away".to_string(),
        column: "done".to_string(),
    });
    state.kanban.placements.push(PlacementEntry {
        host: "local".to_string(),
        session: "kanlocal".to_string(),
        column: "verified".to_string(),
    });
    state.save().expect("seed placements");

    let mut app = App::new(); // refresh runs; ghost errors out
    app.refresh(); // and once more for good measure

    assert_eq!(
        app.kanban_placements
            .get(&(Machine::Remote("ghost".to_string()), "far/away".to_string()))
            .map(String::as_str),
        Some("done"),
        "placement on an unobserved (unreachable) machine must survive"
    );
    assert_eq!(
        app.kanban_placements
            .get(&(Machine::Local, "kanlocal".to_string()))
            .map(String::as_str),
        Some("verified"),
        "live local placement must survive too"
    );
    drop(app);
    drop(tmux);
}

#[cfg(unix)]
#[test]
fn acceptance_kanban_tab_embeds_and_chord_returns_to_board() {
    let _lock = acquire_acceptance_lock();
    let tmux = IsolatedTmux::spawn();
    tmux.new_session("kanembed", 80, 24).expect("new kanembed");
    let _ = poll_for_capture_contains(&tmux, "kanembed", "$ ", Duration::from_secs(2))
        .expect("prompt up");

    let mut app = App::new();
    app.handle_key(k_upper('K'));
    app.handle_key(k('l')); // idle → awaiting, where the card sits
    app.handle_key(k_tab());
    assert!(
        app.embedded_active(),
        "Tab on a focused card should enter embedded mode"
    );
    assert!(
        matches!(app.state, AppState::Kanban(_)),
        "embedding must not leave the kanban state"
    );

    // Type into the embedded session and see it land in the real pane.
    let _ = poll_for_embedded_grid_contains(&app, "$", Duration::from_secs(3))
        .expect("embedded grid shows prompt");
    type_str(&mut app, "echo kanban-embed-ok");
    app.handle_key(k_enter());
    let cap = poll_for_capture_contains(
        &tmux,
        "kanembed",
        "kanban-embed-ok",
        Duration::from_secs(3),
    )
    .expect("typed command reaches the session");
    assert!(cap.contains("kanban-embed-ok"));

    // Exit chord returns to the board, not the tree.
    app.handle_key(k_ctrl_space());
    app.handle_key(k(' '));
    assert!(!app.embedded_active(), "chord should exit embedded mode");
    assert!(
        matches!(app.state, AppState::Kanban(_)),
        "after the chord the user lands back on the board"
    );

    // `p` is an alias for the same immediate-interactive entry — there is
    // no read-only preview state on the board, and it must not flip the
    // tree's persisted preview-pane preference.
    let pane_pref_before = app.preview_pane_enabled;
    app.handle_key(k('p'));
    assert!(
        app.embedded_active(),
        "p should enter the writable embedded state directly"
    );
    assert_eq!(
        app.preview_pane_enabled, pane_pref_before,
        "p on the board must not toggle the tree preview preference"
    );
    app.handle_key(k_ctrl_space());
    app.handle_key(k(' '));
    assert!(!app.embedded_active());
    assert!(matches!(app.state, AppState::Kanban(_)));
    drop(app);
    drop(tmux);
}
