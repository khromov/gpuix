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
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
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

fn schedule<'js>(
    ctx: &Ctx<'js>,
    callback: Function<'js>,
    delay: Option<f64>,
    args: Vec<Value<'js>>,
    repeat: bool,
) -> u32 {
    let delay = Duration::from_secs_f64(delay.unwrap_or(0.0).max(0.0) / 1000.0);
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

fn set_timeout<'js>(ctx: Ctx<'js>, callback: Function<'js>, delay: Opt<f64>, args: Rest<Value<'js>>) -> u32 {
    schedule(&ctx, callback, delay.0, args.0, false)
}

fn set_interval<'js>(ctx: Ctx<'js>, callback: Function<'js>, delay: Opt<f64>, args: Rest<Value<'js>>) -> u32 {
    schedule(&ctx, callback, delay.0, args.0, true)
}

fn clear_timer(id: Opt<u32>) {
    if let Some(id) = id.0 {
        TIMERS.with(|timers| timers.borrow_mut().active.remove(&id));
    }
}

/// One pass over the timers due now. Only those: a callback that schedules a
/// 0 ms timer (the frame loop after a slow frame does) runs it next pass, after
/// the jobs and reloads in between, rather than starving them.
fn run_due_timers(ctx: &Ctx<'_>) {
    let now = Instant::now();
    let mut due: Vec<(u32, Timer)> = TIMERS.with(|timers| {
        let mut timers = timers.borrow_mut();
        let ids: Vec<u32> = timers
            .active
            .iter()
            .filter(|(_, timer)| timer.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| timers.active.remove(&id).map(|timer| (id, timer)))
            .collect()
    });
    due.sort_by_key(|(_, timer)| timer.deadline);

    for (id, timer) in due {
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

fn drain_jobs(ctx: &Ctx<'_>) {
    while ctx.execute_pending_job() {}
}

fn report(ctx: &Ctx<'_>, error: Error, what: &str) {
    let caught: std::result::Result<(), CaughtError<'_>> = Err::<(), _>(error).catch(ctx);
    if let Err(caught) = caught {
        eprintln!("[gpuix-qjs] {what}: {caught}");
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

fn join<'js>(ctx: &Ctx<'js>, args: &[Value<'js>]) -> String {
    args.iter().map(|value| describe(ctx, value)).collect::<Vec<_>>().join(" ")
}

fn console_out<'js>(ctx: Ctx<'js>, args: Rest<Value<'js>>) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", join(&ctx, &args.0));
}

fn console_err<'js>(ctx: Ctx<'js>, args: Rest<Value<'js>>) {
    eprintln!("{}", join(&ctx, &args.0));
}

/// A `napi::Error` becomes the JS exception the napi binding would have thrown.
fn throw(ctx: &Ctx<'_>, error: napi::Error) -> Error {
    Exception::throw_message(ctx, &error.reason)
}

fn to_js<'js, T: serde::Serialize>(ctx: &Ctx<'js>, value: &T) -> Result<Value<'js>> {
    let json = serde_json::to_string(value)
        .map_err(|error| Exception::throw_message(ctx, &format!("serialize: {error}")))?;
    ctx.json_parse(json)
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
                Ok(paths) => resolve.restore(ctx)?.call::<_, ()>((paths,)),
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

    pub fn get_focused_element_id(&self, ctx: Ctx<'_>) -> Result<Option<f64>> {
        self.inner.get_focused_element_id().map_err(|error| throw(&ctx, error))
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

    pub fn get_selected_text(&self) -> Option<String> {
        self.inner.get_selected_text()
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

    pub fn get_list_scroll_top(&self, ctx: Ctx<'_>, element_id: f64) -> Result<Option<Vec<f64>>> {
        self.inner.get_list_scroll_top(element_id).map_err(|error| throw(&ctx, error))
    }

    pub fn get_scroll_offset(&self, ctx: Ctx<'_>, element_id: f64) -> Result<Option<Vec<f64>>> {
        self.inner.get_scroll_offset(element_id).map_err(|error| throw(&ctx, error))
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
    let mut files = HashMap::with_capacity(count as usize);
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

fn split_query(specifier: &str) -> (&str, &str) {
    match specifier.find('?') {
        Some(at) => specifier.split_at(at),
        None => (specifier, ""),
    }
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
        let resolved = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else if path.starts_with('.') {
            let (base_path, _) = split_query(base);
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

const PRELUDE: &str = r#"
(() => {
	const exit = globalThis.__gpuix_exit;
	const write = globalThis.__gpuix_write;
	const info = globalThis.__gpuix_process;
	delete globalThis.__gpuix_exit;
	delete globalThis.__gpuix_write;
	delete globalThis.__gpuix_process;
	const stream = (fd) => ({ write(chunk) { write(fd, String(chunk)); return true; }, isTTY: false });
	globalThis.process = {
		...info,
		exit: (code = 0) => exit(code | 0),
		cwd: () => info.cwd,
		stdout: stream(1),
		stderr: stream(2),
		on() { return this; },
		off() { return this; },
		once() { return this; },
		nextTick: (fn, ...args) => queueMicrotask(() => fn(...args))
	};
	if (typeof globalThis.queueMicrotask !== 'function') {
		globalThis.queueMicrotask = (fn) => { Promise.resolve().then(fn); };
	}
	if (typeof globalThis.structuredClone !== 'function') {
		globalThis.structuredClone = (value) => JSON.parse(JSON.stringify(value));
	}
})();
"#;

fn install_globals(ctx: &Ctx<'_>, argv: &[String]) -> Result<()> {
    let globals = ctx.globals();

    let console = Object::new(ctx.clone())?;
    console.set("log", Func::from(console_out))?;
    console.set("info", Func::from(console_out))?;
    console.set("debug", Func::from(console_out))?;
    console.set("warn", Func::from(console_err))?;
    console.set("error", Func::from(console_err))?;
    console.set("trace", Func::from(console_err))?;
    globals.set("console", console)?;

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
    for (key, value) in std::env::vars() {
        env.set(key, value)?;
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

fn watch_stdin(enabled: bool) -> Receiver<()> {
    let (tx, rx) = mpsc::channel();
    if !enabled {
        return rx;
    }
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) if line.trim() == "reload" => {
                    if tx.send(()).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    rx
}

const USAGE: &str = "usage: gpuix-qjs <bundle.js> [args]\n       gpuix-qjs --pack <dir> --entry <file> --out <executable>\n       gpuix-qjs --version";

fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter().position(|arg| arg == flag).and_then(|at| argv.get(at + 1)).map(String::as_str)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
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
                Ok(path) => path.display().to_string(),
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
    let context = Context::full(&runtime).expect("QuickJS context");
    if let Err(error) = context.with(|ctx| {
        install_globals(&ctx, &argv).map_err(|error| {
            report(&ctx, error, "installing globals failed");
        })
    }) {
        let _ = error;
        std::process::exit(1);
    }

    if !import(&context, &entry) {
        std::process::exit(1);
    }

    // A packed app has no dev server on its stdin, and may have no stdin at all.
    let reloads = watch_stdin(!packed);
    let mut stdin_open = !packed;
    let mut version = 0u32;
    let reload = |version: &mut u32| {
        *version += 1;
        let specifier = format!("{entry}?v={version}");
        eprintln!("[gpuix-qjs] reloading ({specifier})");
        import(&context, &specifier);
    };

    loop {
        context.with(|ctx| {
            run_due_timers(&ctx);
            drain_jobs(&ctx);
        });
        while stdin_open {
            match reloads.try_recv() {
                Ok(()) => reload(&mut version),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => stdin_open = false,
            }
        }

        let next = TIMERS.with(|timers| timers.borrow().next_deadline());
        let pending_jobs = runtime.is_job_pending();
        match next {
            Some(deadline) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                if wait.is_zero() || pending_jobs {
                    continue;
                }
                if stdin_open {
                    match reloads.recv_timeout(wait) {
                        Ok(()) => reload(&mut version),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => stdin_open = false,
                    }
                } else {
                    std::thread::sleep(wait);
                }
            }
            None if pending_jobs => continue,
            None => {
                // Nothing scheduled: only a reload could make the app do anything.
                if !stdin_open {
                    break;
                }
                match reloads.recv() {
                    Ok(()) => reload(&mut version),
                    Err(_) => break,
                }
            }
        }
    }
}
