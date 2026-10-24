//! `gpuix-qjs <bundle.js>`: a QuickJS-ng host for GPUIX apps.
//!
//! The bundle imports `@gpuix/native`, which resolves to a native module here
//! exporting a `GpuixRenderer` with the napi class's method names, so a renderer
//! written against `@gpuix/native` runs unchanged. Everything Node would provide
//! and a GPUIX renderer actually uses — timers, `console`, `process` — is
//! installed as globals. The process is the frame loop's `setTimeout` chain:
//! `main` runs due timers and pending promise jobs until the app calls
//! `process.exit`, which is what the JS frame loop does when `tick()` reports the
//! last window closed.
//!
//! A line `reload` on stdin re-imports the entry with a `?v=N` query, which is
//! how a dev server remounts the app after a rebuild without restarting the
//! window: a bundle split so the Svelte runtime sits in its own chunk keeps that
//! chunk cached, so the remount is one runtime unmounting and mounting again.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::sync::OnceLock;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpuix_native::{EventCallback, EventPayload, PathPromptOptions};
use rquickjs::class::Trace;
use rquickjs::function::Args;
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::{Declarations, Declared, Exports, ModuleDef};
use rquickjs::prelude::{Func, Opt, Rest};
use rquickjs::promise::PromiseState;
use rquickjs::{
    ArrayBuffer, CatchResultExt, CaughtError, Class, Context, Ctx, Error, Exception, Function,
    JsLifetime, Module, Object, Persistent, Promise, Result, Runtime, TypedArray, Value,
};

const NATIVE_MODULE: &str = "@gpuix/native";
const QUICKJS_VERSION: &str = "0.16.2";

// ── Timers ───────────────────────────────────────────────────────────────

struct Timer {
    deadline: Instant,
    interval: Option<Duration>,
    callback: Persistent<Function<'static>>,
    args: Vec<Persistent<Value<'static>>>,
}

#[derive(Default)]
struct Timers {
    next_id: u32,
    active: HashMap<u32, Timer>,
}

impl Timers {
    fn next_deadline(&self) -> Option<Instant> {
        self.active.values().map(|timer| timer.deadline).min()
    }
}

thread_local! {
    static TIMERS: RefCell<Timers> = RefCell::new(Timers::default());
}

/// Node's rule: a delay that is not a finite number in `1..=2^31-1` becomes 1 ms,
/// which also keeps `setInterval(fn, 0)` from spinning the loop.
fn timer_delay(value: Option<Value<'_>>) -> Duration {
    let ms = value.and_then(|value| value.as_number()).unwrap_or(1.0);
    let ms = if ms.is_finite() && ms >= 1.0 && ms <= 2_147_483_647.0 { ms } else { 1.0 };
    Duration::from_secs_f64(ms / 1000.0)
}

fn schedule<'js>(
    ctx: &Ctx<'js>,
    callback: Function<'js>,
    delay: Option<Value<'js>>,
    args: Vec<Value<'js>>,
    repeat: bool,
) -> u32 {
    let delay = timer_delay(delay);
    TIMERS.with(|timers| {
        let mut timers = timers.borrow_mut();
        timers.next_id += 1;
        let id = timers.next_id;
        timers.active.insert(
            id,
            Timer {
                deadline: Instant::now() + delay,
                interval: repeat.then_some(delay),
                callback: Persistent::save(ctx, callback),
                args: args.into_iter().map(|arg| Persistent::save(ctx, arg)).collect(),
            },
        );
        id
    })
}

// `Opt<T>` is `None` only for a missing argument; an explicit `undefined` or
// `null` must not throw the way a typed parameter would, so these take `Value`s.
fn set_timeout<'js>(ctx: Ctx<'js>, callback: Function<'js>, delay: Opt<Value<'js>>, args: Rest<Value<'js>>) -> u32 {
    schedule(&ctx, callback, delay.0, args.0, false)
}

fn set_interval<'js>(ctx: Ctx<'js>, callback: Function<'js>, delay: Opt<Value<'js>>, args: Rest<Value<'js>>) -> u32 {
    schedule(&ctx, callback, delay.0, args.0, true)
}

fn clear_timer(id: Opt<Value<'_>>) {
    if let Some(id) = id.0.and_then(|value| value.as_number()) {
        TIMERS.with(|timers| timers.borrow_mut().active.remove(&(id as u32)));
    }
}

/// One pass over the timers due now. Only those: a callback that schedules a
/// 0 ms timer (the frame loop after a slow frame does) runs it next pass, after
/// the jobs and reloads in between, rather than starving them. Each timer is
/// taken out of the table at its own turn, so a `clearTimeout` from an earlier
/// callback in the same pass still cancels it.
fn run_due_timers(ctx: &Ctx<'_>) {
    let now = Instant::now();
    let mut due: Vec<(Instant, u32)> = TIMERS.with(|timers| {
        timers
            .borrow()
            .active
            .iter()
            .filter(|(_, timer)| timer.deadline <= now)
            .map(|(id, timer)| (timer.deadline, *id))
            .collect()
    });
    due.sort();

    for (_, id) in due {
        let Some(timer) = TIMERS.with(|timers| timers.borrow_mut().active.remove(&id)) else { continue };
        // Re-armed before the callback, so a `clearInterval` inside it is final.
        if let Some(interval) = timer.interval {
            TIMERS.with(|timers| {
                timers.borrow_mut().active.insert(
                    id,
                    Timer {
                        deadline: now + interval,
                        interval: Some(interval),
                        callback: timer.callback.clone(),
                        args: timer.args.clone(),
                    },
                )
            });
        }
        let result = (|| -> Result<()> {
            let callback = timer.callback.restore(ctx)?;
            let mut args = Args::new(ctx.clone(), timer.args.len());
            for arg in timer.args {
                args.push_arg(arg.restore(ctx)?)?;
            }
            args.apply::<()>(&callback)
        })();
        if let Err(error) = result {
            report(ctx, error, "timer callback threw");
        }
        drain_jobs(ctx);
    }
}

// ── Diagnostics ──────────────────────────────────────────────────────────

/// stderr may be a closed pipe; a diagnostic must never take the app down.
fn warn(message: impl std::fmt::Display) {
    let _ = writeln!(std::io::stderr(), "[gpuix-qjs] {message}");
}

/// Runs pending promise jobs and reports the ones that throw. rquickjs's
/// `Ctx::execute_pending_job` catches and drops a job's exception, which would
/// hide every `$effect` that fails after an await (Svelte flushes them in
/// microtasks), so this goes to the engine directly.
fn drain_jobs(ctx: &Ctx<'_>) {
    loop {
        let mut job_ctx = std::ptr::null_mut();
        let runtime = unsafe { rquickjs::qjs::JS_GetRuntime(ctx.as_raw().as_ptr()) };
        let result = unsafe { rquickjs::qjs::JS_ExecutePendingJob(runtime, &mut job_ctx) };
        if result == 0 {
            break;
        }
        if result < 0 {
            // One context per runtime here, so the pending exception is on `ctx`.
            let value = ctx.catch();
            warn(format!("uncaught (in promise job): {}", describe(ctx, &value)));
        }
    }
    // The queue is empty, so a rejection still unhandled has nobody left to handle it.
    for (_, reason) in REJECTIONS.with(|rejections| std::mem::take(&mut *rejections.borrow_mut())) {
        warn(format!("unhandled promise rejection: {reason}"));
    }
}

thread_local! {
    /// Rejected promises nobody has handled yet, by object pointer, with the reason
    /// already described: `drain_jobs` reports what is left once the jobs ran.
    static REJECTIONS: RefCell<Vec<(usize, String)>> = const { RefCell::new(Vec::new()) };
}

fn report(ctx: &Ctx<'_>, error: Error, what: &str) {
    let caught: std::result::Result<(), CaughtError<'_>> = Err::<(), _>(error).catch(ctx);
    if let Err(caught) = caught {
        warn(format!("{what}: {caught}"));
    }
}

fn describe<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> String {
    if let Some(string) = value.as_string() {
        return string.to_string().unwrap_or_default();
    }
    if let Some(exception) = value.as_exception() {
        let mut out = exception.message().unwrap_or_else(|| "Error".to_string());
        if let Some(stack) = exception.stack() {
            if !stack.is_empty() {
                out.push('\n');
                out.push_str(&stack);
            }
        }
        return out;
    }
    if value.is_object() && !value.is_function() {
        if let Ok(Some(json)) = ctx.json_stringify(value.clone()) {
            if let Ok(json) = json.to_string() {
                return json;
            }
        }
    }
    let coerce: Result<Function<'_>> = ctx.globals().get("String");
    coerce
        .and_then(|string| string.call::<_, String>((value.clone(),)))
        .unwrap_or_else(|_| "<value>".to_string())
}

/// `None` is `null`, as the napi binding answers, not rquickjs's `undefined`.
fn nullable<'js, T: rquickjs::IntoJs<'js>>(ctx: &Ctx<'js>, value: Option<T>) -> Result<Value<'js>> {
    match value {
        Some(value) => value.into_js(ctx),
        None => Ok(Value::new_null(ctx.clone())),
    }
}

/// A `napi::Error` becomes the JS exception the napi binding would have thrown.
fn throw(ctx: &Ctx<'_>, error: napi::Error) -> Error {
    Exception::throw_message(ctx, &error.reason)
}

/// Absent `Option` fields are left out, as napi leaves them out, rather than
/// arriving as `null` and defeating `{ x = 0 }` defaults in app code.
fn strip_nulls(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|_, field| !field.is_null());
            map.values_mut().for_each(strip_nulls);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

fn to_js<'js, T: serde::Serialize>(ctx: &Ctx<'js>, value: &T) -> Result<Value<'js>> {
    let mut json =
        serde_json::to_value(value).map_err(|error| Exception::throw_message(ctx, &format!("serialize: {error}")))?;
    strip_nulls(&mut json);
    ctx.json_parse(json.to_string())
}

fn from_js<'js, T: serde::de::DeserializeOwned>(ctx: &Ctx<'js>, value: Option<Value<'js>>) -> Result<Option<T>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    let Some(json) = ctx.json_stringify(value)? else { return Ok(None) };
    serde_json::from_str(&json.to_string()?)
        .map(Some)
        .map_err(|error| Exception::throw_type(ctx, &format!("invalid options: {error}")))
}

fn bytes<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Vec<u8>> {
    if let Ok(array) = TypedArray::<u8>::from_value(value.clone()) {
        if let Some(raw) = array.as_raw() {
            return Ok(unsafe { raw.as_ref() }.to_vec());
        }
    }
    if let Some(buffer) = ArrayBuffer::from_value(value) {
        if let Some(raw) = buffer.as_raw() {
            return Ok(unsafe { raw.as_ref() }.to_vec());
        }
    }
    Err(Exception::throw_type(ctx, "expected a Uint8Array or ArrayBuffer"))
}

// ── The renderer class ───────────────────────────────────────────────────

type PromptResult = std::result::Result<Option<Vec<String>>, String>;

#[derive(Default)]
struct Prompts {
    next_id: Cell<u64>,
    settlers: RefCell<HashMap<u64, (Persistent<Function<'static>>, Persistent<Function<'static>>)>>,
    results: Arc<Mutex<Vec<(u64, PromptResult)>>>,
}

/// `GpuixRenderer` as the napi binding declares it, over `gpuix_native`'s plain
/// Rust constructor. GPUI raises events inside `tick()`'s AppKit pump into a
/// queue, and `tick()` hands them to the JS callback before it returns, so
/// `handle_event` runs with the window idle, as it does under Node.
#[derive(Trace)]
#[rquickjs::class(frozen, rename_all = "camelCase")]
struct GpuixRenderer {
    #[qjs(skip_trace)]
    inner: gpuix_native::GpuixRenderer,
    #[qjs(skip_trace)]
    events: Arc<Mutex<Vec<EventPayload>>>,
    #[qjs(skip_trace)]
    callback: Option<Persistent<Function<'static>>>,
    #[qjs(skip_trace)]
    prompts: Prompts,
}

unsafe impl<'js> JsLifetime<'js> for GpuixRenderer {
    type Changed<'to> = GpuixRenderer;
}

impl GpuixRenderer {
    fn flush<'js>(&self, ctx: &Ctx<'js>) -> Result<()> {
        let events: Vec<EventPayload> = std::mem::take(&mut *self.events.lock().unwrap());
        if !events.is_empty() {
            if let Some(callback) = &self.callback {
                let callback = callback.clone().restore(ctx)?;
                for event in events {
                    let payload = to_js(ctx, &event)?;
                    if let Err(error) = callback.call::<_, ()>((Value::new_null(ctx.clone()), payload)) {
                        report(ctx, error, "event callback threw");
                    }
                }
            }
        }

        let results: Vec<(u64, PromptResult)> = std::mem::take(&mut *self.prompts.results.lock().unwrap());
        for (id, result) in results {
            let Some((resolve, reject)) = self.prompts.settlers.borrow_mut().remove(&id) else { continue };
            let outcome = match result {
                Ok(paths) => resolve.restore(ctx)?.call::<_, ()>((nullable(ctx, paths)?,)),
                Err(reason) => {
                    let error = Exception::from_message(ctx.clone(), &reason)?;
                    reject.restore(ctx)?.call::<_, ()>((error,))
                }
            };
            if let Err(error) = outcome {
                report(ctx, error, "promptForPaths settlement threw");
            }
        }
        Ok(())
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl GpuixRenderer {
    #[qjs(constructor)]
    pub fn new<'js>(ctx: Ctx<'js>, callback: Opt<Value<'js>>) -> Result<Self> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let on_event: EventCallback = Arc::new(move |payload| sink.lock().unwrap().push(payload));
        let callback = match callback.0 {
            Some(value) if value.is_function() => Some(Persistent::save(&ctx, Function::from_value(value)?)),
            _ => None,
        };
        Ok(Self {
            inner: gpuix_native::GpuixRenderer::with_event_callback(Some(on_event)),
            events,
            callback,
            prompts: Prompts::default(),
        })
    }

    pub fn init<'js>(&self, ctx: Ctx<'js>, options: Opt<Value<'js>>) -> Result<()> {
        let options = from_js(&ctx, options.0)?;
        self.inner.init(options).map_err(|error| throw(&ctx, error))
    }

    pub fn apply_batch(&self, ctx: Ctx<'_>, json: String) -> Result<Vec<f64>> {
        self.inner.apply_batch(json).map_err(|error| throw(&ctx, error))
    }

    pub fn tick<'js>(&self, ctx: Ctx<'js>) -> Result<bool> {
        let running = self.inner.tick().map_err(|error| throw(&ctx, error))?;
        self.flush(&ctx)?;
        Ok(running)
    }

    pub fn is_initialized(&self) -> bool {
        self.inner.is_initialized()
    }

    pub fn requires_tick(&self) -> bool {
        self.inner.requires_tick()
    }

    /// The display-link wake needs a thread-safe call into the engine, which
    /// QuickJS has no notion of; the paced `setTimeout` loop is the frame loop.
    pub fn set_frame_wake_callback(&self, _callback: Opt<Value<'_>>) -> bool {
        false
    }

    pub fn get_window_size<'js>(&self, ctx: Ctx<'js>) -> Result<Value<'js>> {
        to_js(&ctx, &self.inner.get_window_size().map_err(|error| throw(&ctx, error))?)
    }

    pub fn get_window_insets<'js>(&self, ctx: Ctx<'js>) -> Result<Value<'js>> {
        to_js(&ctx, &self.inner.get_window_insets().map_err(|error| throw(&ctx, error))?)
    }

    pub fn set_debug_frame_overlay(&self, ctx: Ctx<'_>, mode: String) -> Result<String> {
        self.inner.set_debug_frame_overlay(mode).map_err(|error| throw(&ctx, error))
    }

    pub fn cycle_debug_frame_overlay(&self, ctx: Ctx<'_>) -> Result<String> {
        self.inner.cycle_debug_frame_overlay().map_err(|error| throw(&ctx, error))
    }

    pub fn get_debug_frame_overlay(&self, ctx: Ctx<'_>) -> Result<String> {
        self.inner.get_debug_frame_overlay().map_err(|error| throw(&ctx, error))
    }

    pub fn reset_debug_frame_overlay_stats(&self, ctx: Ctx<'_>) -> Result<()> {
        self.inner.reset_debug_frame_overlay_stats().map_err(|error| throw(&ctx, error))
    }

    pub fn get_debug_frame_overlay_stats<'js>(&self, ctx: Ctx<'js>) -> Result<Value<'js>> {
        to_js(&ctx, &self.inner.get_debug_frame_overlay_stats().map_err(|error| throw(&ctx, error))?)
    }

    pub fn activate_window(&self, ctx: Ctx<'_>) -> Result<()> {
        self.inner.activate_window().map_err(|error| throw(&ctx, error))
    }

    pub fn minimize_window(&self, ctx: Ctx<'_>) -> Result<()> {
        self.inner.minimize_window().map_err(|error| throw(&ctx, error))
    }

    pub fn zoom_window(&self, ctx: Ctx<'_>) -> Result<()> {
        self.inner.zoom_window().map_err(|error| throw(&ctx, error))
    }

    pub fn toggle_fullscreen(&self, ctx: Ctx<'_>) -> Result<()> {
        self.inner.toggle_fullscreen().map_err(|error| throw(&ctx, error))
    }

    pub fn set_window_title(&self, ctx: Ctx<'_>, title: String) -> Result<()> {
        self.inner.set_window_title(title).map_err(|error| throw(&ctx, error))
    }

    /// The picker answers on a background thread, so the result parks in a queue
    /// that the next `tick()` settles the promise from.
    pub fn prompt_for_paths<'js>(&self, ctx: Ctx<'js>, options: Opt<Value<'js>>) -> Result<Promise<'js>> {
        let options: Option<PathPromptOptions> = from_js(&ctx, options.0)?;
        let (promise, resolve, reject) = ctx.promise()?;
        let id = self.prompts.next_id.get() + 1;
        self.prompts.next_id.set(id);
        self.prompts
            .settlers
            .borrow_mut()
            .insert(id, (Persistent::save(&ctx, resolve), Persistent::save(&ctx, reject)));
        let results = self.prompts.results.clone();
        self.inner.prompt_for_paths_with(options, move |result| {
            results.lock().unwrap().push((id, result.map_err(|error| error.reason)));
        });
        Ok(promise)
    }

    pub fn focus_element(&self, ctx: Ctx<'_>, element_id: f64) -> Result<()> {
        self.inner.focus_element(element_id).map_err(|error| throw(&ctx, error))
    }

    pub fn focus_next(&self, ctx: Ctx<'_>) -> Result<()> {
        self.inner.focus_next().map_err(|error| throw(&ctx, error))
    }

    pub fn focus_previous(&self, ctx: Ctx<'_>) -> Result<()> {
        self.inner.focus_previous().map_err(|error| throw(&ctx, error))
    }

    pub fn get_focused_element_id<'js>(&self, ctx: Ctx<'js>) -> Result<Value<'js>> {
        nullable(&ctx, self.inner.get_focused_element_id().map_err(|error| throw(&ctx, error))?)
    }

    pub fn focus_next_within(&self, ctx: Ctx<'_>, element_id: f64) -> Result<()> {
        self.inner.focus_next_within(element_id).map_err(|error| throw(&ctx, error))
    }

    pub fn focus_previous_within(&self, ctx: Ctx<'_>, element_id: f64) -> Result<()> {
        self.inner.focus_previous_within(element_id).map_err(|error| throw(&ctx, error))
    }

    pub fn set_window_key_events(&self, ctx: Ctx<'_>, key_down: bool, key_up: bool, event_id: f64) -> Result<()> {
        self.inner
            .set_window_key_events(key_down, key_up, event_id)
            .map_err(|error| throw(&ctx, error))
    }

    pub fn blur(&self, ctx: Ctx<'_>) -> Result<()> {
        self.inner.blur().map_err(|error| throw(&ctx, error))
    }

    pub fn set_window_selection_change(&self, ctx: Ctx<'_>, enabled: bool, event_id: f64) -> Result<()> {
        self.inner
            .set_window_selection_change(enabled, event_id)
            .map_err(|error| throw(&ctx, error))
    }

    pub fn get_selected_text<'js>(&self, ctx: Ctx<'js>) -> Result<Value<'js>> {
        nullable(&ctx, self.inner.get_selected_text())
    }

    pub fn clear_selection(&self, ctx: Ctx<'_>) -> Result<()> {
        self.inner.clear_selection().map_err(|error| throw(&ctx, error))
    }

    pub fn scroll_to(&self, ctx: Ctx<'_>, element_id: f64, x: f64, y: f64) -> Result<()> {
        self.inner.scroll_to(element_id, x, y).map_err(|error| throw(&ctx, error))
    }

    pub fn scroll_to_item(&self, ctx: Ctx<'_>, element_id: f64, index: f64, offset_in_item: Opt<Option<f64>>) -> Result<()> {
        self.inner
            .scroll_to_item(element_id, index, offset_in_item.0.flatten())
            .map_err(|error| throw(&ctx, error))
    }

    pub fn get_list_scroll_top<'js>(&self, ctx: Ctx<'js>, element_id: f64) -> Result<Value<'js>> {
        nullable(&ctx, self.inner.get_list_scroll_top(element_id).map_err(|error| throw(&ctx, error))?)
    }

    pub fn get_scroll_offset<'js>(&self, ctx: Ctx<'js>, element_id: f64) -> Result<Value<'js>> {
        nullable(&ctx, self.inner.get_scroll_offset(element_id).map_err(|error| throw(&ctx, error))?)
    }

    pub fn scroll_into_view(&self, ctx: Ctx<'_>, element_id: f64) -> Result<()> {
        self.inner.scroll_into_view(element_id).map_err(|error| throw(&ctx, error))
    }

    pub fn set_image_pixels<'js>(
        &self,
        ctx: Ctx<'js>,
        element_id: f64,
        width: f64,
        height: f64,
        pixels: Value<'js>,
        format: Opt<Option<String>>,
    ) -> Result<()> {
        let pixels = bytes(&ctx, pixels)?;
        self.inner
            .set_image_pixels_bytes(element_id, width, height, pixels, format.0.flatten())
            .map_err(|error| throw(&ctx, error))
    }

    pub fn set_image<'js>(&self, ctx: Ctx<'js>, element_id: f64, data: Value<'js>) -> Result<()> {
        let data = bytes(&ctx, data)?;
        self.inner.set_image_bytes(element_id, data).map_err(|error| throw(&ctx, error))
    }

    pub fn register_font<'js>(&self, ctx: Ctx<'js>, data: Value<'js>) -> Result<()> {
        let data = bytes(&ctx, data)?;
        self.inner.register_font_bytes(data).map_err(|error| throw(&ctx, error))
    }

    pub fn get_automation_tree(&self, ctx: Ctx<'_>) -> Result<String> {
        self.inner.get_automation_tree().map_err(|error| throw(&ctx, error))
    }

    pub fn get_element_bounds<'js>(&self, ctx: Ctx<'js>, id: f64) -> Result<Value<'js>> {
        match self.inner.get_element_bounds(id).map_err(|error| throw(&ctx, error))? {
            Some(bounds) => to_js(&ctx, &bounds),
            None => Ok(Value::new_null(ctx)),
        }
    }

    pub fn get_elements_at(&self, ctx: Ctx<'_>, x: f64, y: f64) -> Result<Vec<f64>> {
        self.inner.get_elements_at(x, y).map_err(|error| throw(&ctx, error))
    }

    pub fn get_all_text(&self) -> Vec<String> {
        self.inner.get_all_text()
    }

    pub fn get_painted_text(&self) -> Vec<String> {
        self.inner.get_painted_text()
    }

    pub fn get_painted_highlights<'js>(&self, ctx: Ctx<'js>) -> Result<Value<'js>> {
        to_js(&ctx, &self.inner.get_painted_highlights())
    }

    pub fn simulate_keystrokes(&self, ctx: Ctx<'_>, keystrokes: String) -> Result<()> {
        self.inner.simulate_keystrokes(keystrokes).map_err(|error| throw(&ctx, error))
    }

    pub fn capture_screenshot(&self, ctx: Ctx<'_>, path: String) -> Result<()> {
        self.inner.capture_screenshot(path).map_err(|error| throw(&ctx, error))
    }
}

// ── `@gpuix/native` as a native module ───────────────────────────────────

struct NativeModule;

impl ModuleDef for NativeModule {
    fn declare(declarations: &Declarations<'_>) -> Result<()> {
        declarations.declare("GpuixRenderer")?;
        declarations.declare("isTerminal")?;
        declarations.declare("hasTestGpuixRenderer")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> Result<()> {
        let constructor = Class::<GpuixRenderer>::create_constructor(ctx)?
            .ok_or_else(|| Exception::throw_message(ctx, "GpuixRenderer has no constructor"))?;
        exports.export("GpuixRenderer", constructor)?;
        exports.export("isTerminal", Func::from(gpuix_native::is_terminal))?;
        exports.export("hasTestGpuixRenderer", Func::from(|| false))?;
        Ok(())
    }
}

// ── An app packed into the executable ────────────────────────────────────
//
// `gpuix-qjs --pack <dir> --entry <file> --out <exe>` appends the bundle to a
// copy of this executable: `[files][u64 payload length]["GPUIXQJS"]`, the files
// as `u32 count` then `u32 name length, name, u64 size, bytes` each, with the
// entry's name first as its own length-prefixed string. At startup the trailer
// decides whether argv names a bundle on disk or belongs to the app.

const MAGIC: &[u8; 8] = b"GPUIXQJS";
const BUNDLE_ROOT: &str = "/$bundle/";

struct Bundle {
    entry: String,
    files: HashMap<String, Vec<u8>>,
}

static BUNDLE: OnceLock<Option<Bundle>> = OnceLock::new();

fn read_u32(cursor: &mut &[u8]) -> Option<u32> {
    let (head, rest) = cursor.split_first_chunk::<4>()?;
    *cursor = rest;
    Some(u32::from_le_bytes(*head))
}

fn read_u64(cursor: &mut &[u8]) -> Option<u64> {
    let (head, rest) = cursor.split_first_chunk::<8>()?;
    *cursor = rest;
    Some(u64::from_le_bytes(*head))
}

fn read_bytes<'a>(cursor: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    if cursor.len() < len {
        return None;
    }
    let (head, rest) = cursor.split_at(len);
    *cursor = rest;
    Some(head)
}

fn parse_bundle(mut payload: &[u8]) -> Option<Bundle> {
    let entry_len = read_u32(&mut payload)? as usize;
    let entry = String::from_utf8(read_bytes(&mut payload, entry_len)?.to_vec()).ok()?;
    let count = read_u32(&mut payload)?;
    // The count is untrusted bytes; each entry costs at least its two length fields.
    let mut files = HashMap::with_capacity((count as usize).min(payload.len() / 12));
    for _ in 0..count {
        let name_len = read_u32(&mut payload)? as usize;
        let name = String::from_utf8(read_bytes(&mut payload, name_len)?.to_vec()).ok()?;
        let size = read_u64(&mut payload)? as usize;
        files.insert(name, read_bytes(&mut payload, size)?.to_vec());
    }
    Some(Bundle { entry, files })
}

/// The executable's own length without any payload, and the payload if there is one.
fn split_executable(file: &mut std::fs::File) -> std::io::Result<(u64, Option<Vec<u8>>)> {
    let len = file.metadata()?.len();
    if len < 16 {
        return Ok((len, None));
    }
    let mut trailer = [0u8; 16];
    file.seek(SeekFrom::Start(len - 16))?;
    file.read_exact(&mut trailer)?;
    if &trailer[8..] != MAGIC {
        return Ok((len, None));
    }
    let payload_len = u64::from_le_bytes(trailer[..8].try_into().unwrap());
    if payload_len > len - 16 {
        return Ok((len, None));
    }
    let base = len - 16 - payload_len;
    let mut payload = vec![0u8; payload_len as usize];
    file.seek(SeekFrom::Start(base))?;
    file.read_exact(&mut payload)?;
    Ok((base, Some(payload)))
}

/// A bundle beside the executable: `gpuix-app/` next to it, or in a macOS
/// bundle's `Resources`, with `gpuix-app.json` naming the entry. Appended data
/// fails codesign's strict validation, so this is the form a signed `.app` ships.
fn sidecar_bundle(exe: &Path) -> Option<Bundle> {
    let dir = exe.parent()?;
    for app in [dir.join("gpuix-app"), dir.join("../Resources/gpuix-app")] {
        let Ok(manifest) = std::fs::read_to_string(app.join("gpuix-app.json")) else { continue };
        let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
        let entry = manifest.get("entry")?.as_str()?.to_string();
        let mut files = HashMap::new();
        for item in std::fs::read_dir(&app).ok()? {
            let item = item.ok()?;
            if !item.file_type().ok()?.is_file() {
                continue;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            if name != "gpuix-app.json" {
                files.insert(name, std::fs::read(item.path()).ok()?);
            }
        }
        return Some(Bundle { entry, files });
    }
    None
}

fn embedded_bundle() -> Option<&'static Bundle> {
    BUNDLE
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            let mut file = std::fs::File::open(&exe).ok()?;
            let (_, payload) = split_executable(&mut file).ok()?;
            match payload {
                Some(payload) => parse_bundle(&payload),
                None => sidecar_bundle(&exe),
            }
        })
        .as_ref()
}

fn pack(dir: &Path, entry: &str, out: &Path) -> std::io::Result<()> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(entry.len() as u32).to_le_bytes());
    payload.extend_from_slice(entry.as_bytes());
    let mut names: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|kind| kind.is_file()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    if !names.iter().any(|name| name == entry) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{entry} is not in {}", dir.display()),
        ));
    }
    payload.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for name in &names {
        let bytes = std::fs::read(dir.join(name))?;
        payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        payload.extend_from_slice(&bytes);
    }

    let mut exe = std::fs::File::open(std::env::current_exe()?)?;
    let (base, _) = split_executable(&mut exe)?;
    let mut image = vec![0u8; base as usize];
    exe.seek(SeekFrom::Start(0))?;
    exe.read_exact(&mut image)?;
    image.extend_from_slice(&payload);
    image.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    image.extend_from_slice(MAGIC);

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(out);
    std::fs::write(out, image)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

// ── Module resolution: files by absolute path, a query kept as identity ───

/// The query starts at the first `?` after the last path separator, so a
/// Windows `\\?\` prefix or a `?` in a directory name is not mistaken for one.
fn split_query(specifier: &str) -> (&str, &str) {
    let name_start = specifier.rfind(['/', '\\']).map(|at| at + 1).unwrap_or(0);
    match specifier[name_start..].find('?') {
        Some(at) => specifier.split_at(name_start + at),
        None => (specifier, ""),
    }
}

/// Lexical join for the `/$bundle/` names of a packed app, which are not paths
/// on any platform (`Path` would make them relative on Windows).
fn join_virtual(base: &str, relative: &str) -> String {
    let dir = base.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let mut segments: Vec<&str> = dir.split('/').collect();
    for segment in relative.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.len() > 1 {
                    segments.pop();
                }
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

struct FsResolver;

impl Resolver for FsResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> Result<String> {
        if name == NATIVE_MODULE {
            return Ok(name.to_string());
        }
        let (path, query) = split_query(name);
        let (base_path, _) = split_query(base);
        if path.starts_with(BUNDLE_ROOT) {
            return Ok(name.to_string());
        }
        if path.starts_with('.') && base_path.starts_with(BUNDLE_ROOT) {
            return Ok(format!("{}{}", join_virtual(base_path, path), query));
        }
        let resolved = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else if path.starts_with('.') {
            Path::new(base_path).parent().unwrap_or_else(|| Path::new("")).join(path)
        } else {
            return Err(Error::new_resolving_message(
                base,
                name,
                "only relative and absolute paths resolve here; bundle the app first",
            ));
        };
        Ok(format!("{}{}", normalize(&resolved).display(), query))
    }
}

struct FsLoader;

impl Loader for FsLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> Result<Module<'js, Declared>> {
        if name == NATIVE_MODULE {
            return Module::declare_def::<NativeModule, _>(ctx.clone(), name);
        }
        let (path, _) = split_query(name);
        if let Some(file) = path.strip_prefix(BUNDLE_ROOT) {
            let source = embedded_bundle()
                .and_then(|bundle| bundle.files.get(file))
                .ok_or_else(|| Error::new_loading_message(name, "not in the packed bundle"))?;
            return Module::declare(ctx.clone(), name, source.clone());
        }
        let source = std::fs::read(path).map_err(|error| Error::new_loading_message(name, error.to_string()))?;
        Module::declare(ctx.clone(), name, source)
    }
}

// ── Globals ──────────────────────────────────────────────────────────────

/// The JS half of the globals: everything below the natives (`process`, text
/// coding, `URL`, `fetch` and its classes) is written once, in JS.
const PRELUDE: &str = include_str!("prelude.js");

// ── Promises settled from other threads ──────────────────────────────────
//
// `fetch` answers on a background thread. The thread parks the outcome here
// and the main loop settles the JS promise on its next turn, the same shape as
// `promptForPaths`. The one completion so far is a fetched response, whose body
// becomes a Uint8Array.

enum Completion {
    Fetched {
        status: u16,
        status_text: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
}

type Settler = (Persistent<Function<'static>>, Persistent<Function<'static>>);

static COMPLETIONS: Mutex<Vec<(u64, std::result::Result<Completion, String>)>> = Mutex::new(Vec::new());

thread_local! {
    static SETTLERS: RefCell<HashMap<u64, Settler>> = RefCell::new(HashMap::new());
    static NEXT_SETTLER: Cell<u64> = const { Cell::new(0) };
}

/// A new promise whose resolve/reject wait for a completion under the returned id.
fn deferred<'js>(ctx: &Ctx<'js>) -> Result<(u64, Promise<'js>)> {
    let (promise, resolve, reject) = ctx.promise()?;
    let id = NEXT_SETTLER.with(|next| {
        let id = next.get() + 1;
        next.set(id);
        id
    });
    SETTLERS.with(|settlers| {
        settlers
            .borrow_mut()
            .insert(id, (Persistent::save(ctx, resolve), Persistent::save(ctx, reject)))
    });
    Ok((id, promise))
}

/// What wakes the main loop while it waits: stdin's `reload` line, stdin closing,
/// or a background thread finishing a completion.
enum Wake {
    Reload,
    StdinClosed,
    Completion,
}

static WAKER: OnceLock<mpsc::Sender<Wake>> = OnceLock::new();

fn complete(id: u64, result: std::result::Result<Completion, String>) {
    COMPLETIONS.lock().unwrap().push((id, result));
    if let Some(waker) = WAKER.get() {
        let _ = waker.send(Wake::Completion);
    }
}

fn completions_pending() -> bool {
    !COMPLETIONS.lock().unwrap().is_empty()
}

fn settlers_waiting() -> bool {
    SETTLERS.with(|settlers| !settlers.borrow().is_empty())
}

fn settle_completions(ctx: &Ctx<'_>) {
    let completions: Vec<_> = std::mem::take(&mut *COMPLETIONS.lock().unwrap());
    for (id, result) in completions {
        let Some((resolve, reject)) = SETTLERS.with(|settlers| settlers.borrow_mut().remove(&id)) else { continue };
        let outcome = (|| -> Result<()> {
            match result {
                Ok(Completion::Fetched { status, status_text, url, headers, body }) => {
                    let object = Object::new(ctx.clone())?;
                    object.set("status", status)?;
                    object.set("statusText", status_text)?;
                    object.set("url", url)?;
                    let pairs: Vec<Vec<String>> = headers.into_iter().map(|(name, value)| vec![name, value]).collect();
                    object.set("headers", pairs)?;
                    object.set("body", TypedArray::<u8>::new(ctx.clone(), body)?)?;
                    resolve.restore(ctx)?.call::<_, ()>((object,))
                }
                Err(reason) => {
                    let error = Exception::from_message(ctx.clone(), &reason)?;
                    reject.restore(ctx)?.call::<_, ()>((error,))
                }
            }
        })();
        if let Err(error) = outcome {
            report(ctx, error, "settling a promise threw");
        }
        drain_jobs(ctx);
    }
}

// ── fetch, URL and UTF-8, the natives behind prelude.js ──────────────────

fn http_client() -> std::result::Result<&'static reqwest_client::ReqwestClient, String> {
    static CLIENT: OnceLock<std::result::Result<reqwest_client::ReqwestClient, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| reqwest_client::ReqwestClient::user_agent("gpuix-qjs").map_err(|error| error.to_string()))
        .as_ref()
        .map_err(Clone::clone)
}

#[derive(serde::Deserialize)]
struct FetchInit {
    method: String,
    headers: Vec<(String, String)>,
}

/// `__gpuix_fetch(url, initJson, body?)`: one request on its own thread through
/// the client gpuix-native already links; the whole body is read before the
/// promise settles.
fn fetch<'js>(ctx: Ctx<'js>, url: String, init: String, body: Opt<Value<'js>>) -> Result<Promise<'js>> {
    use futures::AsyncReadExt as _;
    use http_client::HttpClient as _;

    let init: FetchInit = serde_json::from_str(&init).map_err(|error| Exception::throw_type(&ctx, &error.to_string()))?;
    // WHATWG normalisation (spaces, non-ASCII, IDN hosts), which `http::Uri` would reject.
    let url = match http_client::Url::parse(&url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => parsed.to_string(),
        Ok(parsed) => return Err(Exception::throw_type(&ctx, &format!("fetch: unsupported scheme '{}:'", parsed.scheme()))),
        Err(error) => return Err(Exception::throw_type(&ctx, &format!("fetch: invalid URL '{url}': {error}"))),
    };
    let body = match body.0 {
        Some(value) if !value.is_undefined() && !value.is_null() => Some(bytes(&ctx, value)?),
        _ => None,
    };
    let (id, promise) = deferred(&ctx)?;

    std::thread::spawn(move || {
        let outcome = (|| -> std::result::Result<Completion, String> {
            let client = http_client()?;
            let mut request = http_client::Request::builder().method(init.method.as_str()).uri(&url);
            for (name, value) in &init.headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let request = request
                .body(match body {
                    Some(bytes) => http_client::AsyncBody::from(bytes),
                    None => http_client::AsyncBody::empty(),
                })
                .map_err(|error| error.to_string())?;
            let response = futures::executor::block_on(client.send(request)).map_err(|error| format!("{error:#}"))?;
            let status = response.status();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| (name.as_str().to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
                .collect();
            let mut bytes = Vec::new();
            futures::executor::block_on(response.into_body().read_to_end(&mut bytes)).map_err(|error| error.to_string())?;
            Ok(Completion::Fetched {
                status: status.as_u16(),
                status_text: status.canonical_reason().unwrap_or("").to_string(),
                url,
                headers,
                body: bytes,
            })
        })();
        complete(id, outcome.map_err(|reason| format!("fetch failed: {reason}")));
    });
    Ok(promise)
}

/// `__gpuix_url_parse(url, base?)`: the `url` crate's parse, or null when invalid.
fn url_parse<'js>(ctx: Ctx<'js>, url: String, base: Opt<Option<String>>) -> Result<Value<'js>> {
    let parsed = match base.0.flatten() {
        Some(base) => http_client::Url::parse(&base).and_then(|base| base.join(&url)),
        None => http_client::Url::parse(&url),
    };
    let Ok(parsed) = parsed else { return Ok(Value::new_null(ctx)) };
    let object = Object::new(ctx.clone())?;
    object.set("href", parsed.as_str())?;
    object.set("origin", parsed.origin().ascii_serialization())?;
    object.set("protocol", format!("{}:", parsed.scheme()))?;
    object.set("username", parsed.username())?;
    object.set("password", parsed.password().unwrap_or(""))?;
    object.set("hostname", parsed.host_str().unwrap_or(""))?;
    object.set("port", parsed.port().map(|port| port.to_string()).unwrap_or_default())?;
    object.set(
        "host",
        match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            _ => String::new(),
        },
    )?;
    object.set("pathname", parsed.path())?;
    object.set("search", parsed.query().map(|query| format!("?{query}")).unwrap_or_default())?;
    object.set("hash", parsed.fragment().map(|fragment| format!("#{fragment}")).unwrap_or_default())?;
    Ok(object.into_value())
}

/// `__gpuix_open_url(url)`: the platform's browser opener, detached. Only web
/// and mail URLs: the argument comes from app content, and `open` would launch a
/// `file:` app bundle as readily as a page. A failure is reported, not thrown,
/// since the click that asked has already been handled.
fn open_url(url: String) {
    let parsed = match http_client::Url::parse(&url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https" | "mailto") => parsed.to_string(),
        _ => {
            warn(format!("not opening {url:?}: only http, https and mailto URLs are opened"));
            return;
        }
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(&parsed);
        command
    };
    // Not `cmd /C start`: cmd would interpret `&` and `%` inside the URL.
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("rundll32");
        command.args(["url.dll,FileProtocolHandler", &parsed]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(&parsed);
        command
    };
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if let Err(error) = command.spawn() {
        warn(format!("could not open {parsed}: {error}"));
    }
}

fn utf8_encode<'js>(ctx: Ctx<'js>, text: String) -> Result<TypedArray<'js, u8>> {
    TypedArray::new(ctx, text.into_bytes())
}

fn utf8_decode<'js>(ctx: Ctx<'js>, data: Value<'js>) -> Result<String> {
    Ok(String::from_utf8_lossy(&bytes(&ctx, data)?).into_owned())
}

fn install_globals(ctx: &Ctx<'_>, argv: &[String]) -> Result<()> {
    let globals = ctx.globals();

    globals.set("setTimeout", Func::from(set_timeout))?;
    globals.set("setInterval", Func::from(set_interval))?;
    globals.set("clearTimeout", Func::from(clear_timer))?;
    globals.set("clearInterval", Func::from(clear_timer))?;

    globals.set(
        "__gpuix_exit",
        Func::from(|code: Opt<i32>| -> () {
            let _ = std::io::stdout().flush();
            std::process::exit(code.0.unwrap_or(0));
        }),
    )?;
    globals.set(
        "__gpuix_write",
        Func::from(|fd: i32, chunk: String| {
            if fd == 2 {
                let _ = std::io::stderr().write_all(chunk.as_bytes());
            } else {
                let _ = std::io::stdout().write_all(chunk.as_bytes());
            }
        }),
    )?;

    let info = Object::new(ctx.clone())?;
    let env = Object::new(ctx.clone())?;
    // `vars()` panics on a non-UTF-8 value; a stray byte in the environment is not the app's problem.
    for (key, value) in std::env::vars_os() {
        env.set(key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())?;
    }
    info.set("env", env)?;
    info.set("argv", argv.to_vec())?;
    // Node's names, which is what app code compares against.
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    info.set("platform", platform)?;
    info.set("arch", std::env::consts::ARCH)?;
    info.set("pid", std::process::id())?;
    info.set(
        "cwd",
        std::env::current_dir().map(|dir| dir.display().to_string()).unwrap_or_default(),
    )?;
    let versions = Object::new(ctx.clone())?;
    versions.set("quickjs", QUICKJS_VERSION)?;
    versions.set("gpuixQjs", env!("CARGO_PKG_VERSION"))?;
    info.set("versions", versions)?;
    globals.set("__gpuix_process", info)?;

    globals.set("__gpuix_fetch", Func::from(fetch))?;
    globals.set("__gpuix_url_parse", Func::from(url_parse))?;
    globals.set("__gpuix_utf8_encode", Func::from(utf8_encode))?;
    globals.set("__gpuix_utf8_decode", Func::from(utf8_decode))?;
    globals.set("__gpuix_open_url", Func::from(open_url))?;

    ctx.eval::<(), _>(PRELUDE)
}

// ── Main loop ────────────────────────────────────────────────────────────

/// Imports the entry (or a `?v=N` re-import of it), runs the jobs that settles
/// it, and reports a rejection. A module still pending afterwards is awaiting a
/// timer, which the loop will deliver.
fn import(context: &Context, specifier: &str) -> bool {
    context.with(|ctx| {
        let promise = match Module::import(&ctx, specifier) {
            Ok(promise) => promise,
            Err(error) => {
                report(&ctx, error, &format!("loading {specifier} failed"));
                return false;
            }
        };
        drain_jobs(&ctx);
        match promise.state() {
            PromiseState::Rejected => {
                if let Some(Err(error)) = promise.result::<Value>() {
                    report(&ctx, error, &format!("{specifier} threw"));
                }
                false
            }
            _ => true,
        }
    })
}

fn watch_stdin(waker: mpsc::Sender<Wake>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) if line.trim() == "reload" => {
                    if waker.send(Wake::Reload).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = waker.send(Wake::StdinClosed);
    });
}

const USAGE: &str = "usage: gpuix-qjs <bundle.js> [args]\n       gpuix-qjs --pack <dir> --entry <file> --out <executable>\n       gpuix-qjs --version";

fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter().position(|arg| arg == flag).and_then(|at| argv.get(at + 1)).map(String::as_str)
}

/// `canonicalize` on Windows answers a `\\?\C:\…` verbatim path, which the
/// resolver's `?` split and `Path::is_absolute` both misread.
fn plain_path(path: PathBuf) -> String {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => rest.to_string(),
        _ => text,
    }
}

fn main() {
    let argv: Vec<String> = std::env::args_os().map(|arg| arg.to_string_lossy().into_owned()).collect();
    let packed = embedded_bundle().is_some();
    let entry = if let Some(bundle) = embedded_bundle() {
        format!("{BUNDLE_ROOT}{}", bundle.entry)
    } else {
        match argv.get(1).map(String::as_str) {
            Some("--version") => {
                println!("gpuix-qjs {} (quickjs-ng {QUICKJS_VERSION})", env!("CARGO_PKG_VERSION"));
                return;
            }
            Some("--pack") => {
                let (Some(dir), Some(entry), Some(out)) =
                    (argv.get(2).filter(|arg| !arg.starts_with('-')), flag_value(&argv, "--entry"), flag_value(&argv, "--out"))
                else {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                };
                if let Err(error) = pack(Path::new(dir), entry, Path::new(out)) {
                    eprintln!("[gpuix-qjs] pack failed: {error}");
                    std::process::exit(1);
                }
                return;
            }
            Some(path) if !path.starts_with('-') => match std::fs::canonicalize(path) {
                Ok(path) => plain_path(path),
                Err(error) => {
                    eprintln!("[gpuix-qjs] {path}: {error}");
                    std::process::exit(2);
                }
            },
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    };

    let runtime = Runtime::new().expect("QuickJS runtime");
    runtime.set_loader(FsResolver, FsLoader);
    // Node prints these; silence here would hide every rejected `fetch` nobody caught.
    // The engine reports a rejection before a `.then` written right after it has
    // attached, then reports it handled, so the verdict waits for the job drain.
    runtime.set_host_promise_rejection_tracker(Some(Box::new(|ctx, promise, reason, handled| {
        let key = unsafe { rquickjs::qjs::JS_VALUE_GET_PTR(promise.as_raw()) } as usize;
        REJECTIONS.with(|rejections| {
            let mut rejections = rejections.borrow_mut();
            if handled {
                rejections.retain(|(other, _)| *other != key);
            } else {
                let reason = describe(&ctx, &reason);
                rejections.push((key, reason));
            }
        });
    })));
    let context = Context::full(&runtime).expect("QuickJS context");
    // Node's shape, `[runtime, script, ...args]`: a packed app has no script path, so its
    // virtual entry fills the slot and `argv.slice(2)` means the same thing everywhere.
    let mut js_argv = argv.clone();
    if packed {
        js_argv.insert(1, entry.clone());
    }
    if let Err(error) = context.with(|ctx| {
        install_globals(&ctx, &js_argv).map_err(|error| {
            report(&ctx, error, "installing globals failed");
        })
    }) {
        let _ = error;
        std::process::exit(1);
    }

    if !import(&context, &entry) {
        std::process::exit(1);
    }

    let (waker, wakes) = mpsc::channel();
    WAKER.set(waker.clone()).ok().expect("one main");
    // stdin is the dev server's reload channel and nothing else, so it is read only
    // when `GPUIX_QJS_DEV` says one is attached: a backgrounded `gpuix-qjs app.js &`
    // reading a terminal would be stopped with SIGTTIN, window and all.
    let dev = !packed && std::env::var_os("GPUIX_QJS_DEV").is_some_and(|value| !value.is_empty() && value != "0");
    let mut stdin_open = dev;
    if dev {
        watch_stdin(waker);
    }
    let mut version = 0u32;
    let mut handle = |wake: Wake, stdin_open: &mut bool| match wake {
        Wake::Reload => {
            version += 1;
            let specifier = format!("{entry}?v={version}");
            warn(format!("reloading ({specifier})"));
            import(&context, &specifier);
        }
        // The dev server went away; a window it can no longer reach should not linger.
        Wake::StdinClosed => {
            *stdin_open = false;
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        // Settled at the top of the next turn.
        Wake::Completion => {}
    };

    loop {
        context.with(|ctx| {
            run_due_timers(&ctx);
            settle_completions(&ctx);
            drain_jobs(&ctx);
        });
        while let Ok(wake) = wakes.try_recv() {
            handle(wake, &mut stdin_open);
        }

        let next = TIMERS.with(|timers| timers.borrow().next_deadline());
        let ready = runtime.is_job_pending() || completions_pending();
        match next {
            Some(deadline) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                if wait.is_zero() || ready {
                    continue;
                }
                match wakes.recv_timeout(wait) {
                    Ok(wake) => handle(wake, &mut stdin_open),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => unreachable!("WAKER keeps a sender"),
                }
            }
            None if ready => continue,
            None => {
                // Nothing scheduled and nothing in flight: only a reload could make
                // the app do anything, and a closed stdin rules that out too.
                if !settlers_waiting() && !stdin_open {
                    break;
                }
                match wakes.recv() {
                    Ok(wake) => handle(wake, &mut stdin_open),
                    Err(_) => break,
                }
            }
        }
    }
    // Not a return: dropping the runtime with a live Persistent callback would
    // trip QuickJS's leak assertion, and there is nothing left to clean up anyway.
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}
