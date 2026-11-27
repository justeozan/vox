//! Dictation mode: speak, and the cleaned-up text lands at the cursor of the
//! frontmost app — the Amical (github.com/amicalhq/amical, MIT) loop, ported.
//!
//! The dictation key (Fn by default — see hotkey.rs) held → record; released
//! → transcribe, format, paste. A double tap latches hands-free until the next
//! press; a single short tap does nothing; Esc cancels; a chord (Fn+←) drops it
//! silently. Separate from ⌥Space, which talks to the assistant.
//!
//! Pipeline: frontmost-app context (Accessibility) captured at key-down →
//! native recording with pre-roll (capture.rs) → STT primed with the
//! vocabulary → optional LLM formatting (Amical's prompt) → replacements →
//! boundary spacing → clipboard + ⌘V, the previous clipboard restored after.

use crate::dictation_text as dt;
use crate::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

/// Held at least this long → push-to-talk; shorter is a tap.
const HOLD: Duration = Duration::from_millis(400);
/// A second press within this of a tap latches hands-free.
const DOUBLE_TAP: Duration = Duration::from_millis(450);
/// Pill + sound only after this: a chord (Fn+←) is over by then and stays invisible.
const SHOW_AFTER: Duration = Duration::from_millis(140);
/// Shorter recordings are taps or noise.
const MIN_AUDIO: Duration = Duration::from_millis(250);
/// The formatter is a nicety: past this, paste the raw transcript.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(12);
/// Editing a selection needs the model: give it more room than the formatter.
const EDIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Amical's value: long enough for the target app to read the pasteboard.
const CLIPBOARD_RESTORE_DELAY: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub app_name: String,
    pub bundle_id: String,
    pub url: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
    /// Non-empty selected text (edit mode); never read from secure fields.
    pub selected: Option<String>,
    /// A selection exists but is too long to edit: never dictate over it.
    pub selection_too_long: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    /// Key down, recording.
    Holding { since: Instant },
    /// Released quickly: waiting to see whether a second press follows.
    Tapped { at: Instant },
    HandsFree,
    Processing,
}

struct Session {
    phase: Phase,
    ctx: Option<Context>,
    /// Bumped per dictation so late timers / context captures from an old one
    /// can't touch the next.
    gen: u64,
    shown: bool,
    last_text: Option<String>,
}

static SESSION: Mutex<Session> =
    Mutex::new(Session { phase: Phase::Idle, ctx: None, gen: 0, shown: false, last_text: None });
static APP: OnceLock<(AppHandle, Arc<AppState>)> = OnceLock::new();

pub fn init(app: AppHandle, state: Arc<AppState>) {
    let _ = APP.set((app, state));
}

fn app() -> Option<&'static (AppHandle, Arc<AppState>)> {
    APP.get()
}

pub fn is_recording() -> bool {
    matches!(SESSION.lock().unwrap().phase, Phase::Holding { .. } | Phase::Tapped { .. } | Phase::HandsFree)
}

// ── Key state machine ────────────────────────────────────────────────────────

/// The dictation key went down / up (event tap, or the ⌥⇧Space shortcut).
pub fn on_key(down: bool) {
    let mut s = SESSION.lock().unwrap();
    match (s.phase, down) {
        (Phase::Idle, true) => {
            s.gen += 1;
            s.ctx = None;
            s.shown = false;
            s.phase = Phase::Holding { since: Instant::now() };
            let gen = s.gen;
            drop(s);
            begin(gen);
        }
        (Phase::Holding { since }, false) => {
            if since.elapsed() >= HOLD {
                s.phase = Phase::Processing;
                drop(s);
                finish();
            } else {
                // Keep recording through the double-tap window: if it becomes
                // hands-free, the first words aren't lost.
                s.phase = Phase::Tapped { at: Instant::now() };
                let gen = s.gen;
                drop(s);
                std::thread::spawn(move || {
                    std::thread::sleep(DOUBLE_TAP);
                    let s = SESSION.lock().unwrap();
                    if s.gen == gen && matches!(s.phase, Phase::Tapped { .. }) {
                        drop(s);
                        abort(true);
                    }
                });
            }
        }
        (Phase::Tapped { at }, true) if at.elapsed() < DOUBLE_TAP => {
            s.phase = Phase::HandsFree;
            let gen = s.gen;
            drop(s);
            println!("[vox] dictation: hands-free");
            show(gen);
            if let Some((app, _)) = app() {
                let _ = app.emit("dictation-latched", ());
            }
        }
        (Phase::HandsFree, true) => {
            s.phase = Phase::Processing;
            drop(s);
            finish();
        }
        _ => {}
    }
}

/// Esc while recording.
pub fn cancel() {
    if is_recording() {
        println!("[vox] dictation: cancelled");
        abort(false);
    }
}

/// Another key while the dictation key is held: it was a shortcut (Fn+←,
/// right-⌘+Tab…), not a dictation.
pub fn chord() {
    if matches!(SESSION.lock().unwrap().phase, Phase::Holding { .. }) {
        abort(true);
    }
}

fn abort(quiet: bool) {
    let mut s = SESSION.lock().unwrap();
    if !matches!(s.phase, Phase::Holding { .. } | Phase::Tapped { .. } | Phase::HandsFree) {
        return;
    }
    s.phase = Phase::Idle;
    s.ctx = None;
    let shown = std::mem::take(&mut s.shown);
    drop(s);
    crate::capture::cancel();
    if shown {
        if !quiet {
            play_cue("Bottle");
        }
        if let Some((app, _)) = app() {
            let _ = app.emit("dictation-cancel", ());
        }
    }
}

fn begin(gen: u64) {
    crate::capture::start();
    // Context off this thread (it is the event tap's): AX calls into the target
    // app can take a few hundred ms. The frontmost app is what matters now.
    std::thread::spawn(move || {
        let ctx = capture_context();
        let mut s = SESSION.lock().unwrap();
        if s.gen == gen {
            s.ctx = Some(ctx);
        }
    });
    std::thread::spawn(move || {
        std::thread::sleep(SHOW_AFTER);
        show(gen);
    });
}

/// Pill, sound and level meter — once per dictation, and only while it is live.
fn show(gen: u64) {
    {
        let mut s = SESSION.lock().unwrap();
        if s.gen != gen || s.shown || !matches!(s.phase, Phase::Holding { .. } | Phase::HandsFree) {
            return;
        }
        s.shown = true;
    }
    let Some((app, state)) = app() else { return };
    let app_name = frontmost_app().map(|(n, _, _)| n).unwrap_or_default();
    let key = state.settings.lock().unwrap().dictation_key.clone();
    play_cue("Tink");
    println!("[vox] dictation: start in {app_name:?}");
    let _ = app.emit("dictation-start", json!({ "app": app_name, "key": key }));
    let app = app.clone();
    std::thread::spawn(move || loop {
        {
            let s = SESSION.lock().unwrap();
            if s.gen != gen || !matches!(s.phase, Phase::Holding { .. } | Phase::Tapped { .. } | Phase::HandsFree) {
                return;
            }
        }
        let _ = app.emit("dictation-level", json!({ "level": crate::capture::level() }));
        std::thread::sleep(Duration::from_millis(33));
    });
}

fn finish() {
    let Some((app, state)) = app() else { return };
    let shown = SESSION.lock().unwrap().shown;
    if !shown {
        // Released before the pill even appeared (between SHOW_AFTER and HOLD
        // is impossible, so this is a race) — treat as a tap.
        return abort(true);
    }
    play_cue("Pop");
    println!("[vox] dictation: stop");
    let _ = app.emit("dictation-stop", ());
    let (app, state) = (app.clone(), state.clone());
    std::thread::spawn(move || {
        let samples = crate::capture::stop().unwrap_or_default();
        process(&app, &state, &samples);
    });
}

fn play_cue(name: &str) {
    let path = format!("/System/Library/Sounds/{name}.aiff");
    let _ = std::process::Command::new("/usr/bin/afplay").args(["-v", "0.3", &path]).spawn();
}

// ── Processing ───────────────────────────────────────────────────────────────

fn done(app: &AppHandle, ok: bool, text: &str, error: Option<String>) {
    {
        let mut s = SESSION.lock().unwrap();
        s.phase = Phase::Idle;
        s.shown = false;
    }
    if let Some(e) = &error {
        println!("[vox] dictation: not typed — {e}");
    }
    let _ = app.emit("dictation-done", json!({ "ok": ok, "text": text, "error": error }));
}

/// 16 kHz mono samples from capture.rs. Always ends with a `dictation-done`.
fn process(app: &AppHandle, state: &Arc<AppState>, samples: &[f32]) {
    let (ctx, previous) = {
        let mut s = SESSION.lock().unwrap();
        (s.ctx.take().unwrap_or_default(), s.last_text.clone())
    };
    let fr = state.settings.lock().unwrap().language.starts_with("fr");
    let msg = |en: &str, fr_msg: &str| if fr { fr_msg.to_string() } else { en.to_string() };

    let min = (crate::capture::OUT_RATE as f64 * MIN_AUDIO.as_secs_f64()) as usize;
    if samples.len() < min {
        return done(app, false, "", Some(msg("Too short — nothing to type.", "Trop court, rien à écrire.")));
    }
    let tmp = std::env::temp_dir().join(format!("vox_dictation_{}.wav", crate::now_ms_u64()));
    if crate::capture::write_wav(&tmp, samples, crate::capture::OUT_RATE).is_err() {
        return done(app, false, "", Some("cannot write audio".into()));
    }

    let vocab = load_vocabulary();
    let prompt = dt::whisper_prompt(&vocab.words, previous.as_deref().or(ctx.before.as_deref()));
    let t = Instant::now();
    let raw = crate::speech::transcribe_with_prompt(state, &tmp, Some(&prompt));
    let _ = std::fs::remove_file(&tmp);
    let raw = raw.trim().to_string();
    println!("[vox] dictation: transcribed {:.1}s of audio in {:?}", samples.len() as f64 / 16000.0, t.elapsed());
    if raw.is_empty() {
        return done(app, false, "", Some(msg("Didn't catch anything.", "Je n'ai rien entendu.")));
    }

    if ctx.selection_too_long {
        // Pasting the dictated instruction would overwrite the selection.
        return done(
            app,
            false,
            "",
            Some(msg(
                "Selection too long to edit — left unchanged.",
                "Sélection trop longue pour être modifiée — rien n'a changé.",
            )),
        );
    }
    if is_edit_mode(&ctx) {
        return edit_selection(app, state, &raw, &ctx, fr);
    }

    let use_llm = state.settings.lock().unwrap().dictation_format == "llm";
    let formatted = if use_llm { format_with_llm(state, &raw, &ctx, &vocab.words, fr) } else { None };
    let was_formatted = formatted.is_some();
    let text = formatted.unwrap_or_else(|| raw.clone());
    let pairs: Vec<(String, String)> = vocab.replacements.iter().map(|r| (r.from.clone(), r.to.clone())).collect();
    let text = strip_hesitations(&text);
    let text = dt::apply_replacements(&text, &pairs);
    let text = dt::normalize_boundaries(&text, ctx.before.as_deref(), ctx.after.as_deref());
    if text.trim().is_empty() {
        return done(app, false, "", Some(msg("Didn't catch anything.", "Je n'ai rien entendu.")));
    }
    println!("[vox] dictation → {:?} ({} chars, formatted={was_formatted})", ctx.app_name, text.len());

    SESSION.lock().unwrap().last_text = Some(text.trim().to_string());
    append_history(&ctx, &raw, &text, was_formatted);

    if delivery_for(accessibility_trusted(false)) == Delivery::ClipboardOnly {
        // No Accessibility → no synthetic ⌘V. Leave it on the clipboard instead
        // of losing it.
        set_clipboard_text(&text);
        return done(
            app,
            false,
            &text,
            Some(msg(DENIED_MSG.1, DENIED_MSG.0)),
        );
    }
    paste(app, &text);
    done(app, true, text.trim(), None);
}

/// A non-blank selection turns the dictation into a voice command on it.
fn is_edit_mode(ctx: &Context) -> bool {
    ctx.selected.as_deref().is_some_and(|s| !s.trim().is_empty())
}

/// Voice command on the selection: the transcript is an instruction, the LLM
/// returns the edited text, ⌘V replaces the selection. Never pastes on doubt.
fn edit_selection(app: &AppHandle, state: &Arc<AppState>, instruction: &str, ctx: &Context, fr: bool) {
    let msg = |en: &str, fr_msg: &str| if fr { fr_msg.to_string() } else { en.to_string() };
    let selection = ctx.selected.clone().unwrap_or_default();
    let system = dt::build_edit_system(fr);
    let user = dt::edit_user(&selection, instruction);
    let model = state.settings.lock().unwrap().model.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::llm::chat_once(&model, &system, &user, 4000));
    });
    let failed = || {
        msg(
            "Couldn't edit the selection — left unchanged.",
            "Modification impossible — la sélection n'a pas changé.",
        )
    };
    let out = rx.recv_timeout(EDIT_TIMEOUT).ok().flatten();
    let Some(edited) = dt::finish_edit(&selection, out.as_deref()) else {
        return done(app, false, "", Some(failed()));
    };
    println!("[vox] dictation edit → {:?} ({} → {} chars)", ctx.app_name, selection.len(), edited.len());
    append_history(ctx, instruction, &edited, true);
    if delivery_for(accessibility_trusted(false)) == Delivery::ClipboardOnly {
        set_clipboard_text(&edited);
        return done(app, false, &edited, Some(msg(DENIED_MSG.1, DENIED_MSG.0)));
    }
    paste(app, &edited);
    done(app, true, edited.trim(), None);
}

fn format_with_llm(state: &Arc<AppState>, raw: &str, ctx: &Context, vocab: &[String], fr: bool) -> Option<String> {
    let app_type = dt::detect_app_type(&ctx.bundle_id, ctx.url.as_deref());
    let system = dt::build_formatter_system(app_type, vocab, ctx.before.as_deref(), ctx.after.as_deref(), fr);
    let user = dt::formatter_user(raw);
    let model = state.settings.lock().unwrap().model.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::llm::chat_once(&model, &system, &user, 2000));
    });
    let out = match rx.recv_timeout(FORMAT_TIMEOUT) {
        Ok(Some(out)) => out,
        Ok(None) => {
            eprintln!("[vox] dictation: formatter returned nothing — using raw transcript");
            return None;
        }
        Err(_) => {
            eprintln!("[vox] dictation: formatter timed out — using raw transcript");
            return None;
        }
    };
    let text = dt::extract_formatted_text(&out, raw);
    // A small local model sometimes answers the dictation instead of
    // formatting it. Amical's prompt fights that; this is the backstop.
    if text.chars().count() > raw.chars().count() * 2 + 40 {
        eprintln!("[vox] dictation: formatter output much longer than input — using raw transcript");
        return None;
    }
    Some(text)
}

/// Pure hesitations a small formatter keeps anyway ("euh, est-ce que…").
/// Only sounds that are never words — "ben", "genre", "like" stay with the LLM.
fn strip_hesitations(text: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(^|[\s(«\x22])(?:euh+|heu+|hum+|hmm+|um+|uh+)\b[,.…]*\s*").unwrap()
    });
    let out = re.replace_all(text, "$1").to_string();
    // A sentence that started with the hesitation keeps its capital.
    let was_upper = text.trim_start().chars().next().is_some_and(char::is_uppercase);
    let lead = out.len() - out.trim_start().len();
    let mut chars = out[lead..].chars();
    match chars.next() {
        Some(c) if was_upper && c.is_lowercase() => format!("{}{}{}", &out[..lead], c.to_uppercase(), chars.as_str()),
        _ => out,
    }
}

/// Toast shown when Accessibility is missing: (FR, EN). Says what to do, since a
/// reinstall / new signature leaves a stale (checked but invalid) entry.
const DENIED_MSG: (&str, &str) = (
    "Accessibilité non accordée — copié dans le presse-papiers. Retire puis remets Vox dans Réglages Système → Confidentialité et sécurité → Accessibilité.",
    "Accessibility not granted — copied to the clipboard. Remove and re-add Vox in System Settings → Privacy & Security → Accessibility.",
);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Delivery {
    Paste,
    ClipboardOnly,
}

/// Without Accessibility there is no synthetic ⌘V: keep the text on the clipboard.
fn delivery_for(trusted: bool) -> Delivery {
    if trusted { Delivery::Paste } else { Delivery::ClipboardOnly }
}

/// Combine the two Accessibility probes. `AXIsProcessTrusted` can lag behind a
/// fresh grant, so when it says no, a second non-intrusive probe (`tap`, run
/// lazily) gets the last word. Returns (trusted, tap result if it was run).
/// Neither probe ever prompts.
pub fn resolve_trust(ax_trusted: bool, tap: impl FnOnce() -> bool) -> (bool, Option<bool>) {
    if ax_trusted {
        return (true, None);
    }
    let t = tap();
    (t, Some(t))
}

#[cfg(test)]
mod trust_tests {
    use super::*;

    #[test]
    fn ax_trusted_skips_the_tap_probe() {
        let (ok, tap) = resolve_trust(true, || panic!("tap must not run"));
        assert!(ok);
        assert_eq!(tap, None);
    }

    #[test]
    fn stale_ax_but_tap_ok_is_trusted() {
        assert_eq!(resolve_trust(false, || true), (true, Some(true)));
    }

    #[test]
    fn both_failing_is_untrusted() {
        assert_eq!(resolve_trust(false, || false), (false, Some(false)));
    }

    #[test]
    fn edit_mode_needs_a_non_blank_selection() {
        let mut c = Context::default();
        assert!(!is_edit_mode(&c));
        c.selected = Some("  \n".into());
        assert!(!is_edit_mode(&c));
        c.selected = Some("bonjour".into());
        assert!(is_edit_mode(&c));
        assert!(!c.selection_too_long);
    }

    #[test]
    fn missing_permission_keeps_text_on_clipboard() {
        assert_eq!(delivery_for(false), Delivery::ClipboardOnly);
        assert_eq!(delivery_for(true), Delivery::Paste);
    }

    #[test]
    fn denied_message_explains_the_fix_in_both_languages() {
        for m in [DENIED_MSG.0, DENIED_MSG.1] {
            assert!(m.contains("Accessib"));
            assert!(m.contains("Vox"));
            assert!(m.contains("→"));
        }
    }
}

#[cfg(test)]
mod hesitation_tests {
    #[test]
    fn strips_only_hesitations() {
        use super::strip_hesitations as s;
        assert_eq!(s("euh, est-ce que tu peux"), "est-ce que tu peux");
        assert_eq!(s("Euh, est-ce que tu peux"), "Est-ce que tu peux");
        assert_eq!(s("je pense, euh, que oui"), "je pense, que oui");
        assert_eq!(s("Um, I think so"), "I think so");
        assert_eq!(s("Umbrella and human"), "Umbrella and human");
        assert_eq!(s("ben oui"), "ben oui");
    }
}

// ── Vocabulary & history ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Replacement {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Vocabulary {
    #[serde(default)]
    pub words: Vec<String>,
    #[serde(default)]
    pub replacements: Vec<Replacement>,
}

fn vocabulary_path() -> PathBuf {
    crate::home().join(".vox/vocabulary.json")
}

pub fn load_vocabulary() -> Vocabulary {
    std::fs::read_to_string(vocabulary_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_vocabulary(v: Vocabulary) -> Result<(), String> {
    let mut words: Vec<String> = Vec::new();
    for w in v.words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
        if !words.iter().any(|x| x.eq_ignore_ascii_case(w)) {
            words.push(w.to_string());
        }
    }
    let replacements: Vec<Replacement> = v
        .replacements
        .into_iter()
        .map(|r| Replacement { from: r.from.trim().to_string(), to: r.to.trim().to_string() })
        .filter(|r| !r.from.is_empty())
        .collect();
    let p = vocabulary_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let body = serde_json::to_string_pretty(&Vocabulary { words, replacements }).map_err(|e| e.to_string())?;
    std::fs::write(p, body).map_err(|e| e.to_string())
}

fn append_history(ctx: &Context, raw: &str, text: &str, formatted: bool) {
    use std::io::Write;
    let p = crate::home().join(".vox/dictations.jsonl");
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let line: Value = json!({
        "ts": crate::now_ms_u64(),
        "app": ctx.app_name,
        "bundle_id": ctx.bundle_id,
        "raw": raw,
        "text": text,
        "formatted": formatted,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{line}");
    }
}

// ── macOS: frontmost app, Accessibility context, clipboard, ⌘V ───────────────

#[cfg(target_os = "macos")]
mod mac {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    use std::ffi::c_void;

    pub type CFTypeRef = *const c_void;
    type AXUIElementRef = *const c_void;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct CFRange {
        pub location: isize,
        pub length: isize,
    }

    const K_AX_VALUE_CFRANGE_TYPE: u32 = 4;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(el: AXUIElementRef, attr: CFTypeRef, out: *mut CFTypeRef) -> i32;
        fn AXUIElementSetAttributeValue(el: AXUIElementRef, attr: CFTypeRef, value: CFTypeRef) -> i32;
        fn AXUIElementSetMessagingTimeout(el: AXUIElementRef, secs: f32) -> i32;
        fn AXUIElementCopyParameterizedAttributeValue(
            el: AXUIElementRef,
            attr: CFTypeRef,
            param: CFTypeRef,
            out: *mut CFTypeRef,
        ) -> i32;
        fn AXValueCreate(ty: u32, value: *const c_void) -> CFTypeRef;
        fn AXValueGetValue(v: CFTypeRef, ty: u32, out: *mut c_void) -> u8;
        fn AXIsProcessTrusted() -> u8;
        fn AXIsProcessTrustedWithOptions(opts: CFTypeRef) -> u8;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: CFTypeRef);
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFNumberGetValue(n: CFTypeRef, ty: isize, out: *mut c_void) -> u8;
        fn CFArrayGetTypeID() -> usize;
        fn CFArrayGetCount(a: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(a: CFTypeRef, i: isize) -> CFTypeRef;
        fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
        static kCFBooleanTrue: CFTypeRef;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceCreate(state: i32) -> CFTypeRef;
        fn CGEventCreateKeyboardEvent(src: CFTypeRef, key: u16, down: bool) -> CFTypeRef;
        fn CGEventSetFlags(ev: CFTypeRef, flags: u64);
        fn CGEventSetIntegerValueField(ev: CFTypeRef, field: u32, value: i64);
        fn CGEventPost(tap: u32, ev: CFTypeRef);
    }

    /// Owned CF object, released on drop.
    struct Cf(CFTypeRef);
    impl Drop for Cf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) }
            }
        }
    }

    fn attr(el: CFTypeRef, name: &str) -> Option<Cf> {
        let key = NSString::from_str(name);
        let mut out: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(el, Retained::as_ptr(&key) as CFTypeRef, &mut out) };
        (err == 0 && !out.is_null()).then(|| Cf(out))
    }

    fn cf_string(v: &Cf) -> Option<String> {
        unsafe {
            if CFGetTypeID(v.0) != CFStringGetTypeID() {
                return None;
            }
            let ns = &*(v.0 as *const NSString);
            Some(ns.to_string())
        }
    }

    fn str_attr(el: CFTypeRef, name: &str) -> Option<String> {
        attr(el, name).and_then(|v| cf_string(&v))
    }

    fn param_attr(el: CFTypeRef, name: &str, param: CFTypeRef) -> Option<Cf> {
        let key = NSString::from_str(name);
        let mut out: CFTypeRef = std::ptr::null();
        let err = unsafe {
            AXUIElementCopyParameterizedAttributeValue(el, Retained::as_ptr(&key) as CFTypeRef, param, &mut out)
        };
        (err == 0 && !out.is_null()).then(|| Cf(out))
    }

    fn cf_index(v: &Cf) -> Option<isize> {
        unsafe {
            if CFGetTypeID(v.0) != CFNumberGetTypeID() {
                return None;
            }
            let mut n: isize = 0;
            // kCFNumberCFIndexType = 14
            (CFNumberGetValue(v.0, 14, &mut n as *mut _ as *mut c_void) != 0).then_some(n)
        }
    }

    /// Elements of a CF array attribute, each retained.
    fn array_attr(el: CFTypeRef, name: &str) -> Vec<Cf> {
        let Some(arr) = attr(el, name) else { return Vec::new() };
        unsafe {
            if CFGetTypeID(arr.0) != CFArrayGetTypeID() {
                return Vec::new();
            }
            (0..CFArrayGetCount(arr.0))
                .map(|i| Cf(CFRetain(CFArrayGetValueAtIndex(arr.0, i))))
                .collect()
        }
    }

    /// Selection as (UTF-16 start, length) through WebKit/Chromium text
    /// markers — the only selection API many Electron and web editors answer.
    fn selection_via_markers(el: CFTypeRef) -> Option<(usize, usize)> {
        let range = attr(el, "AXSelectedTextMarkerRange")
            .or_else(|| array_attr(el, "AXSelectedTextMarkerRanges").into_iter().next())?;
        let start = param_attr(el, "AXStartTextMarkerForTextMarkerRange", range.0)?;
        let end = param_attr(el, "AXEndTextMarkerForTextMarkerRange", range.0)?;
        let a = cf_index(&param_attr(el, "AXIndexForTextMarker", start.0)?)?;
        let b = cf_index(&param_attr(el, "AXIndexForTextMarker", end.0)?)?;
        (a >= 0 && b >= a).then(|| (a as usize, (b - a) as usize))
    }

    fn selection_via_range(el: CFTypeRef) -> Option<(usize, usize)> {
        let r = attr(el, "AXSelectedTextRange")?;
        let mut out = CFRange::default();
        let ok = unsafe { AXValueGetValue(r.0, K_AX_VALUE_CFRANGE_TYPE, &mut out as *mut _ as *mut c_void) != 0 };
        (ok && out.location >= 0).then(|| (out.location as usize, out.length.max(0) as usize))
    }

    /// The element's text: AXValue, else AXStringForRange over all of it.
    fn content(el: CFTypeRef) -> Option<String> {
        let text = str_attr(el, "AXValue").or_else(|| {
            let n = cf_index(&attr(el, "AXNumberOfCharacters")?)?;
            if n <= 0 {
                return Some(String::new());
            }
            let range = CFRange { location: 0, length: n.min(50_000) };
            let param = Cf(unsafe { AXValueCreate(K_AX_VALUE_CFRANGE_TYPE, &range as *const _ as *const c_void) });
            param_attr(el, "AXStringForRange", param.0).and_then(|v| cf_string(&v))
        })?;
        if placeholder_showing(el, &text) {
            return Some(String::new());
        }
        Some(text)
    }

    fn norm(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
    }

    fn dom_classes(el: CFTypeRef) -> Vec<String> {
        array_attr(el, "AXDOMClassList").iter().filter_map(cf_string).collect()
    }

    /// An empty field often reports its placeholder as its value — Amical's
    /// checks: AXPlaceholderValue, Quill's `ql-blank` (plus ProseMirror/TipTap's
    /// `is-editor-empty`), or a descendant whose DOM class says placeholder and
    /// whose text is the value.
    fn placeholder_showing(el: CFTypeRef, value: &str) -> bool {
        let v = norm(value);
        if v.is_empty() {
            return false;
        }
        if str_attr(el, "AXPlaceholderValue").is_some_and(|p| norm(&p) == v) {
            return true;
        }
        if dom_classes(el).iter().any(|c| c == "ql-blank" || c == "is-editor-empty" || c == "is-empty") {
            return true;
        }
        let mut queue: std::collections::VecDeque<(Cf, u8)> =
            array_attr(el, "AXChildren").into_iter().map(|c| (c, 1)).collect();
        let mut seen = 0;
        while let Some((node, depth)) = queue.pop_front() {
            seen += 1;
            if seen > 60 {
                break;
            }
            if dom_classes(node.0).iter().any(|c| c.to_lowercase().contains("placeholder")) {
                let text = str_attr(node.0, "AXValue")
                    .or_else(|| str_attr(node.0, "AXTitle"))
                    .or_else(|| str_attr(node.0, "AXDescription"))
                    .or_else(|| {
                        array_attr(node.0, "AXChildren").first().and_then(|c| str_attr(c.0, "AXValue"))
                    });
                if text.is_some_and(|t| norm(&t) == v) {
                    return true;
                }
            }
            if depth < 4 {
                queue.extend(array_attr(node.0, "AXChildren").into_iter().take(10).map(|c| (c, depth + 1)));
            }
        }
        false
    }

    const TEXT_ROLES: &[&str] = &["AXTextArea", "AXTextField", "AXComboBox", "AXSearchField"];

    /// Nearest AXWebArea above the element (Chromium often focuses a wrapper).
    fn web_area(el: CFTypeRef) -> Option<Cf> {
        let mut cur = attr(el, "AXParent")?;
        for _ in 0..12 {
            if str_attr(cur.0, "AXRole").as_deref() == Some("AXWebArea") {
                return Some(cur);
            }
            cur = attr(cur.0, "AXParent")?;
        }
        None
    }

    /// First text-capable descendant (bounded BFS) — the focus can land on a
    /// container while the caret lives in a child.
    fn text_descendant(el: CFTypeRef) -> Option<Cf> {
        let mut queue: std::collections::VecDeque<Cf> = array_attr(el, "AXChildren").into();
        let mut seen = 0;
        while let Some(node) = queue.pop_front() {
            seen += 1;
            if seen > 150 {
                break;
            }
            let role = str_attr(node.0, "AXRole").unwrap_or_default();
            if TEXT_ROLES.contains(&role.as_str()) || attr(node.0, "AXSelectedTextMarkerRange").is_some() {
                return Some(node);
            }
            queue.extend(array_attr(node.0, "AXChildren").into_iter().take(12));
        }
        None
    }

    pub fn frontmost_app() -> Option<(String, String, i32)> {
        unsafe {
            let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
            if ws.is_null() {
                return None;
            }
            let app: *mut AnyObject = msg_send![ws, frontmostApplication];
            if app.is_null() {
                return None;
            }
            let pid: i32 = msg_send![app, processIdentifier];
            let name: *mut NSString = msg_send![app, localizedName];
            let bundle: *mut NSString = msg_send![app, bundleIdentifier];
            let s = |p: *mut NSString| if p.is_null() { String::new() } else { (*p).to_string() };
            Some((s(name), s(bundle), pid))
        }
    }

    extern "C" fn probe_cb(_p: *mut c_void, _t: u32, ev: *mut c_void, _u: *mut c_void) -> *mut c_void {
        ev
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            mask: u64,
            cb: extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void,
            user: *mut c_void,
        ) -> CFTypeRef;
    }

    /// Non-intrusive probe: an active tap can only be created when Accessibility
    /// is effectively granted. It is never attached to a run loop and is
    /// released at once, so it neither fires nor conflicts with hotkey.rs.
    fn tap_probe() -> bool {
        unsafe {
            let mask: u64 = 1 << 12; // flagsChanged
            let tap = CGEventTapCreate(1, 0, 0, mask, probe_cb, std::ptr::null_mut());
            if tap.is_null() {
                return false;
            }
            CFRelease(tap);
            true
        }
    }

    static LAST_PROBE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

    pub fn trusted(prompt: bool) -> bool {
        unsafe {
            let ax = AXIsProcessTrusted() != 0;
            let (ok, tap) = super::resolve_trust(ax, tap_probe);
            // Log only on change: this is polled while the settings panel is open.
            let code = (ax as u8) | (tap.unwrap_or(false) as u8) << 1 | 4;
            if LAST_PROBE.swap(code, std::sync::atomic::Ordering::Relaxed) != code {
                println!("[vox] accessibility probes: AXIsProcessTrusted={ax} eventTap={tap:?}");
            }
            if ok {
                return true;
            }
            if prompt {
                let key = NSString::from_str("AXTrustedCheckOptionPrompt");
                let yes: *mut AnyObject = msg_send![class!(NSNumber), numberWithBool: true];
                let dict: *mut AnyObject = msg_send![class!(NSDictionary), dictionaryWithObject: yes, forKey: &*key];
                return AXIsProcessTrustedWithOptions(dict as CFTypeRef) != 0;
            }
            false
        }
    }

    /// Up to 500 UTF-16 units each side of the selection — Amical's window.
    const CONTEXT_UNITS: usize = 500;

    pub fn capture() -> super::Context {
        let Some((app_name, bundle_id, pid)) = frontmost_app() else {
            return Default::default();
        };
        let mut ctx = super::Context { app_name, bundle_id, ..Default::default() };
        if !trusted(false) {
            return ctx;
        }
        unsafe {
            let app = Cf(AXUIElementCreateApplication(pid));
            if app.0.is_null() {
                return ctx;
            }
            AXUIElementSetMessagingTimeout(app.0, 0.25);
            // Chromium/Electron only build their AX tree on request.
            let manual = NSString::from_str("AXManualAccessibility");
            AXUIElementSetAttributeValue(app.0, Retained::as_ptr(&manual) as CFTypeRef, kCFBooleanTrue);

            if let Some(win) = attr(app.0, "AXFocusedWindow") {
                ctx.url = str_attr(win.0, "AXDocument")
                    .or_else(|| str_attr(win.0, "AXURL"))
                    .filter(|u| !u.is_empty());
            }
            let Some(focused) = attr(app.0, "AXFocusedUIElement") else { return ctx };
            if str_attr(focused.0, "AXRole").as_deref() == Some("AXSecureTextField")
                || str_attr(focused.0, "AXSubrole").as_deref() == Some("AXSecureTextField")
            {
                return ctx;
            }
            // Amical's order: the focused element, its web area, a text child —
            // text markers first on each (Chromium/Electron), then the classic
            // AXSelectedTextRange.
            let mut candidates = vec![focused];
            if let Some(w) = web_area(candidates[0].0) {
                candidates.push(w);
            }
            if let Some(d) = text_descendant(candidates[0].0) {
                candidates.push(d);
            }
            let mut found: Option<(String, usize, usize)> = None;
            for el in &candidates {
                let Some((start, len)) = selection_via_markers(el.0).or_else(|| selection_via_range(el.0)) else {
                    continue;
                };
                if let Some(text) = content(el.0) {
                    found = Some((text, start, len));
                    break;
                }
            }
            // No caret information at all: assume it is at the end.
            let (value, start, len) = match found {
                Some(f) => f,
                None => match content(candidates[0].0) {
                    Some(t) => {
                        let n = t.encode_utf16().count();
                        (t, n, 0)
                    }
                    None => return ctx,
                },
            };
            let units: Vec<u16> = value.encode_utf16().collect();
            let start = start.min(units.len());
            let end = (start + len).min(units.len());
            let pre = &units[start.saturating_sub(CONTEXT_UNITS)..start];
            let post = &units[end..(end + CONTEXT_UNITS).min(units.len())];
            ctx.before = Some(String::from_utf16_lossy(pre));
            ctx.after = Some(String::from_utf16_lossy(post));
            ctx.selected = crate::dictation_text::selection_text(&units, start, len);
            ctx.selection_too_long = len > crate::dictation_text::MAX_SELECTION_UNITS;
        }
        ctx
    }

    // NSPasteboard. Snapshot keeps every type of every item, so an image or a
    // file reference on the clipboard survives the round-trip, not just text.
    pub type Snapshot = Vec<Vec<(Retained<NSString>, Retained<AnyObject>)>>;

    unsafe fn pasteboard() -> *mut AnyObject {
        msg_send![class!(NSPasteboard), generalPasteboard]
    }

    pub fn change_count() -> isize {
        unsafe { msg_send![pasteboard(), changeCount] }
    }

    pub fn snapshot() -> Snapshot {
        let mut out = Vec::new();
        unsafe {
            let pb = pasteboard();
            let items: *mut AnyObject = msg_send![pb, pasteboardItems];
            if items.is_null() {
                return out;
            }
            let n: usize = msg_send![items, count];
            for i in 0..n {
                let item: *mut AnyObject = msg_send![items, objectAtIndex: i];
                let types: *mut AnyObject = msg_send![item, types];
                if types.is_null() {
                    continue;
                }
                let tn: usize = msg_send![types, count];
                let mut entry = Vec::new();
                for j in 0..tn {
                    let ty: *mut NSString = msg_send![types, objectAtIndex: j];
                    let data: *mut AnyObject = msg_send![item, dataForType: ty];
                    if let (Some(ty), Some(data)) = (Retained::retain(ty), Retained::retain(data)) {
                        entry.push((ty, data));
                    }
                }
                if !entry.is_empty() {
                    out.push(entry);
                }
            }
        }
        out
    }

    pub fn restore(snap: Snapshot) {
        unsafe {
            let pb = pasteboard();
            let _: isize = msg_send![pb, clearContents];
            if snap.is_empty() {
                return;
            }
            let arr: *mut AnyObject = msg_send![class!(NSMutableArray), array];
            for entry in &snap {
                let item: *mut AnyObject = msg_send![class!(NSPasteboardItem), new];
                for (ty, data) in entry {
                    let _: bool = msg_send![item, setData: &**data, forType: &**ty];
                }
                let _: () = msg_send![arr, addObject: item];
                // `new` returned +1; the array holds its own reference now.
                let _: () = msg_send![item, release];
            }
            let _: bool = msg_send![pb, writeObjects: arr];
        }
    }

    pub fn set_text(text: &str) -> isize {
        unsafe {
            let pb = pasteboard();
            let _: isize = msg_send![pb, clearContents];
            let s = NSString::from_str(text);
            let ty = NSString::from_str("public.utf8-plain-text");
            let _: bool = msg_send![pb, setString: &*s, forType: &*ty];
            msg_send![pb, changeCount]
        }
    }

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> CFTypeRef;
        fn TISCopyCurrentASCIICapableKeyboardInputSource() -> CFTypeRef;
        fn TISGetInputSourceProperty(src: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        static kTISPropertyUnicodeKeyLayoutData: CFTypeRef;
        fn LMGetKbdType() -> u8;
        fn UCKeyTranslate(
            layout: *const c_void,
            keycode: u16,
            action: u16,
            modifiers: u32,
            kbd_type: u32,
            options: u32,
            dead_key_state: *mut u32,
            max_len: usize,
            actual_len: *mut usize,
            chars: *mut u16,
        ) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
    }

    /// Keycode that types `ch` with ⌘ held under the active layout — Amical's
    /// KeyboardLayoutResolver. Keycode 9 is "v" on QWERTY/AZERTY/QWERTZ but
    /// "k" on Dvorak, and Chromium apps read the ⌘-translated character.
    /// The current source first, then its ASCII-capable companion for
    /// non-Latin layouts. TIS is main-thread only.
    pub fn keycode_for(ch: char) -> Option<u16> {
        let target = ch.to_lowercase().next()? as u32;
        if target > 0xFFFF {
            return None;
        }
        unsafe {
            for copy in [TISCopyCurrentKeyboardInputSource, TISCopyCurrentASCIICapableKeyboardInputSource] {
                let src = Cf(copy());
                if src.0.is_null() {
                    continue;
                }
                let data = TISGetInputSourceProperty(src.0, kTISPropertyUnicodeKeyLayoutData);
                if data.is_null() {
                    continue;
                }
                let layout = CFDataGetBytePtr(data) as *const c_void;
                let kbd = LMGetKbdType() as u32;
                for kc in 0u16..128 {
                    let mut dead = 0u32;
                    let mut len = 0usize;
                    let mut buf = [0u16; 4];
                    // action 3 = kUCKeyActionDisplay; modifiers 1 = cmdKey >> 8;
                    // options 1 = kUCKeyTranslateNoDeadKeysMask
                    let st = UCKeyTranslate(layout, kc, 3, 1, kbd, 1, &mut dead, 4, &mut len, buf.as_mut_ptr());
                    if st == 0 && len == 1 && buf[0] as u32 == target {
                        return Some(kc);
                    }
                }
            }
        }
        None
    }

    /// ⌘ + `key`. Flags are set explicitly on every event so a still-held ⌥
    /// or ⇧ can't turn it into ⌥⌘V, and each event carries Vox's tag so the
    /// dictation-key tap ignores it.
    pub fn cmd_v(key_v: u16) {
        const KEY_V_FALLBACK: u16 = 9;
        let key_v = if key_v == 0xFFFF { KEY_V_FALLBACK } else { key_v };
        const KEY_CMD: u16 = 55;
        const FLAG_CMD: u64 = 0x0010_0000;
        unsafe {
            let src = Cf(CGEventSourceCreate(1)); // HIDSystemState
            for (key, down, flags) in [
                (KEY_CMD, true, FLAG_CMD),
                (key_v, true, FLAG_CMD),
                (key_v, false, FLAG_CMD),
                (KEY_CMD, false, 0),
            ] {
                let ev = Cf(CGEventCreateKeyboardEvent(src.0, key, down));
                if ev.0.is_null() {
                    continue;
                }
                CGEventSetFlags(ev.0, flags);
                CGEventSetIntegerValueField(ev.0, crate::hotkey::FIELD_SOURCE_USER_DATA, crate::hotkey::SELF_TAG);
                CGEventPost(1, ev.0); // kCGSessionEventTap
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn frontmost_app() -> Option<(String, String, i32)> {
    mac::frontmost_app()
}
#[cfg(target_os = "macos")]
fn capture_context() -> Context {
    mac::capture()
}
#[cfg(target_os = "macos")]
pub fn accessibility_trusted(prompt: bool) -> bool {
    mac::trusted(prompt)
}
#[cfg(target_os = "macos")]
fn set_clipboard_text(text: &str) {
    mac::set_text(text);
}
#[cfg(target_os = "macos")]
fn paste(app: &AppHandle, text: &str) {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = app.run_on_main_thread(move || {
        let _ = tx.send(mac::keycode_for('v'));
    });
    let key_v = rx.recv_timeout(Duration::from_millis(500)).ok().flatten().unwrap_or(0xFFFF);
    // Retained ObjC objects aren't Send; these are only ever touched again by
    // the restore thread, after this one has let go of them.
    struct SendSnap(mac::Snapshot);
    unsafe impl Send for SendSnap {}
    impl SendSnap {
        // A method call makes the closure capture the whole wrapper; a
        // destructuring pattern would capture only the non-Send field.
        fn take(self) -> mac::Snapshot {
            self.0
        }
    }
    let snap = SendSnap(mac::snapshot());
    let ours = mac::set_text(text);
    std::thread::sleep(Duration::from_millis(30));
    mac::cmd_v(key_v);
    std::thread::spawn(move || {
        std::thread::sleep(CLIPBOARD_RESTORE_DELAY);
        // Only if nobody copied anything since — never clobber a newer copy.
        if mac::change_count() == ours {
            mac::restore(snap.take());
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn frontmost_app() -> Option<(String, String, i32)> {
    None
}
#[cfg(not(target_os = "macos"))]
fn capture_context() -> Context {
    Context::default()
}
#[cfg(not(target_os = "macos"))]
pub fn accessibility_trusted(_prompt: bool) -> bool {
    false
}
#[cfg(not(target_os = "macos"))]
fn set_clipboard_text(_text: &str) {}
#[cfg(not(target_os = "macos"))]
fn paste(_app: &AppHandle, _text: &str) {}

#[cfg(all(test, target_os = "macos"))]
mod live_tests {
    //! Touch the real clipboard / AX / Ollama — run by hand:
    //! `cargo test --lib dictation::live_tests -- --ignored --nocapture`
    use super::*;

    #[test]
    #[ignore]
    fn clipboard_round_trip() {
        let before = mac::snapshot();
        let n = before.len();
        let ours = mac::set_text("vox dictation test");
        assert_eq!(mac::change_count(), ours);
        mac::restore(before);
        assert_eq!(mac::snapshot().len(), n);
    }

    /// Activate TextEdit with an empty document first.
    #[test]
    #[ignore]
    fn paste_into_front_app() {
        std::thread::sleep(Duration::from_millis(300));
        let ours = mac::set_text("collé par vox");
        std::thread::sleep(Duration::from_millis(30));
        mac::cmd_v(mac::keycode_for('v').unwrap_or(9));
        std::thread::sleep(Duration::from_millis(300));
        println!("changeCount unchanged by paste: {}", mac::change_count() == ours);
    }

    #[test]
    #[ignore]
    fn layout_v() {
        println!("v = keycode {:?}", mac::keycode_for('v'));
    }

    #[test]
    #[ignore]
    fn context_capture() {
        println!("trusted={} ctx={:?}", mac::trusted(false), mac::capture());
    }

    #[test]
    #[ignore]
    fn ollama_formats_without_answering() {
        let raw = "euh est-ce que tu peux euh m'envoyer le le rapport avant vendredi s'il te plaît";
        let sys = dt::build_formatter_system(dt::AppType::Chat, &["Orca".into()], Some("Salut Marc, "), Some(""), true);
        let body = json!({"model": "qwen2.5:3b", "stream": false, "options": {"temperature": 0.1},
            "messages": [{"role": "system", "content": sys}, {"role": "user", "content": dt::formatter_user(raw)}]});
        let t = Instant::now();
        let v: Value = ureq::post("http://127.0.0.1:11434/api/chat").send_json(body).unwrap().into_json().unwrap();
        let out = v["message"]["content"].as_str().unwrap();
        let text = dt::extract_formatted_text(out, raw);
        println!("{:?} → {:?} in {:?}", raw, dt::normalize_boundaries(&text, Some("Salut Marc, "), Some("")), t.elapsed());
    }
}
