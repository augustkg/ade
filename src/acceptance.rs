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

use crate::app::App;
use crate::test_harness::{
    acquire_acceptance_lock, poll_for_capture_contains, poll_until, IsolatedTmux,
};

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
    app.handle_key(k('q'));
    assert!(
        !app.embedded_active(),
        "chord then q should exit embedded mode"
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
    app.handle_key(k_ctrl_space());
    app.handle_key(k('q'));
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

