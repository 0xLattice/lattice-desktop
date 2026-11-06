//! Keeping the app running while a swap needs it. Swaps run in this process
//! (see `swap`), so while one is going the app must not stop:
//!
//! - **Needs the app:** a swap run is still going (its engine thread, or its
//!   last status report), a swap's status says `mustStayOpen`, or, before
//!   the wallet opens, the wallet on disk has a swap to resume. Nothing is
//!   decided while the wallet is being reopened.
//! - **Tray** (Tauri's `TrayIconBuilder`, the `tray-icon` feature): built
//!   the first time a swap needs the app and shown only while one does,
//!   with Open, the swap's status line and Quit.
//! - **Close to tray:** closing the window then hides it; Open, the macOS
//!   dock (`RunEvent::Reopen`) and a second launch (tauri-plugin-single-
//!   instance) show it again. Once no swap needs the app, a window closed
//!   this way quits the app as closing it would have. With no tray (Linux
//!   without an appindicator library), the close asks first, as a quit.
//! - **Quit confirm:** a quit (the app menu's Quit, Cmd+Q, the tray's Quit,
//!   the last window closing, and on macOS the Dock's Quit, a logout or a
//!   restart) while a swap needs the app is held for a native confirm
//!   (tauri-plugin-dialog) that names the refund window.
//! - **Notifications** (tauri-plugin-notification) at a swap's steps that
//!   matter, once per swap and step.
//! - **Launch at login** (tauri-plugin-autostart): on while a swap needs the
//!   app, off once none does, unless it was on before this app turned it on.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_autostart::{AutoLaunchManager, ManagerExt as _};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;

use crate::swap::Activity;
use crate::{app_root, err, Result};

/// How often the engine is looked at for runs starting and ending.
const TICK: Duration = Duration::from_secs(1);

/// A step a swap is notified at, once per swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Notice {
    /// The BTC lock has its confirmations: the swap is under way.
    Locked,
    /// The swap is cancelled and the BTC refund is under way.
    Refunding,
    /// The seller took the BTC; the NEX was asked for, and is again at
    /// each start.
    Punished,
    Refunded,
    /// The NEX has arrived.
    Received,
    /// A punished swap got its NEX after all.
    Recovered,
}

impl Notice {
    fn text(self) -> (&'static str, &'static str) {
        match self {
            Notice::Locked => (
                "Bitcoin locked",
                "Your BTC lock is confirmed. Keep Lattice running until the swap finishes.",
            ),
            Notice::Refunding => (
                "Swap cancelled",
                "Refunding your BTC. Keep Lattice running until the refund confirms.",
            ),
            Notice::Punished => (
                "Swap punished",
                "The seller took the BTC. If it locked NEX, Lattice asks it to release the NEX, and asks again each time it starts.",
            ),
            Notice::Refunded => ("BTC refunded", "Your BTC is back in your swap wallet."),
            Notice::Received => ("NEX received", "Your NEX is in your address."),
            Notice::Recovered => (
                "NEX recovered",
                "The seller took the BTC, and Lattice claimed your NEX.",
            ),
        }
    }
}

/// The notice `status` calls for, if any; `punished` whether the swap was
/// punished before.
fn notice(status: &Value, punished: bool) -> Option<Notice> {
    let locked = matches!(
        (status["confirmations"]["funding"].as_u64(), status["btc_confirmations"].as_u64()),
        (Some(depth), Some(needed)) if depth >= needed
    );
    match (status["step"].as_str()?, status["outcome"].as_str()) {
        ("received", _) | (_, Some("swapped")) if punished => Some(Notice::Recovered),
        ("received", _) | (_, Some("swapped")) => Some(Notice::Received),
        (_, Some("refunded")) => Some(Notice::Refunded),
        (_, Some("punished")) | ("asking", _) if !punished => Some(Notice::Punished),
        ("refunding", _) => Some(Notice::Refunding),
        ("swapping" | "finishing", _) if locked && !punished => Some(Notice::Locked),
        _ => None,
    }
}

/// Whether a swap needs the app: a run still going, or a status saying the
/// app must stay open.
fn needs_app(busy: bool, last: &HashMap<String, Value>) -> bool {
    busy || last.values().any(|status| status["mustStayOpen"] == true)
}

/// The tray's line for a swap's `status`.
fn status_line(status: Option<&Value>) -> String {
    let Some(status) = status else {
        return "Swap in progress".into();
    };
    let depth = status["confirmations"]["funding"].as_u64();
    let needed = status["btc_confirmations"].as_u64();
    match (
        status["step"].as_str().unwrap_or_default(),
        status["outcome"].as_str(),
    ) {
        (_, Some("swapped")) => "Swap complete: NEX received".into(),
        (_, Some("refunded")) => "Swap ended: BTC refunded".into(),
        (_, Some("punished")) => "Swap punished: asking the seller for the NEX".into(),
        (_, Some(outcome)) => format!("Swap ended: {outcome}"),
        ("fund-wallet", _) => "Swap: waiting for your BTC deposit".into(),
        ("swapping", _) => match (depth, needed) {
            (Some(depth), Some(needed)) if depth < needed => {
                format!("Swap: BTC lock {depth} of {needed} confirmations")
            }
            _ => "Swap: in progress".into(),
        },
        ("asking", _) => "Swap punished: asking the seller for the NEX".into(),
        ("finishing", _) => "Swap: claiming your NEX".into(),
        ("received", _) => match status["confirmations"]["nexLock"].as_u64() {
            Some(1) => "Swap: NEX received · 1 Nexus confirmation".into(),
            Some(depth) => format!("Swap: NEX received · {depth} Nexus confirmations"),
            None => "Swap: NEX received".into(),
        },
        ("refunding", _) => "Swap: refunding your BTC".into(),
        ("cancelled", _) => "Swap cancelled".into(),
        ("offer-gone", _) => "Swap stopped: the offer is gone".into(),
        _ => "Swap in progress".into(),
    }
}

/// What a quit confirm says: the earliest refund deadline among `statuses`,
/// if any is known, with `now` in milliseconds since the epoch.
fn quit_warning<'a>(statuses: impl Iterator<Item = &'a Value>, now: u64) -> String {
    let punish = statuses
        .filter_map(|status| {
            let punish = &status["deadlines"]["punish"];
            Some((punish["height"].as_u64()?, punish["eta"].as_u64()?))
        })
        .min();
    let Some((height, eta)) = punish else {
        return "A swap is in progress, and it runs only while Lattice runs. \
                If you quit, it stops until you open Lattice again."
            .into();
    };
    let hours = eta.saturating_sub(now) / 3_600_000;
    let when = match hours {
        0 => "within the hour".to_string(),
        1 => "in about 1 hour".to_string(),
        n => format!("in about {n} hours"),
    };
    format!(
        "A swap is in progress, and it runs only while Lattice runs. If the swap \
         is cancelled, Lattice must refund your BTC before Bitcoin block {height} \
         ({when}); after that the seller can take it. Quit only if you will open \
         Lattice again well before then."
    )
}

/// What the shell knows of the swaps, from their status reports and the
/// engine.
#[derive(Default)]
pub(crate) struct Tracker {
    /// Each held swap's last status.
    last: HashMap<String, Value>,
    /// The followed swap, whose status the tray shows.
    followed: Option<String>,
    /// Swaps known to have been punished.
    punished: HashSet<String>,
    /// Notices already shown.
    notified: HashSet<(String, Notice)>,
    /// Whether a swap needs the app, as last decided; none before the first
    /// look at the engine.
    needs: Option<bool>,
}

impl Tracker {
    /// A swap's status report: the notice it calls for, if not already shown.
    pub(crate) fn report(&mut self, status: &Value) -> Option<Notice> {
        let id = status["swapId"].as_str()?.to_string();
        // Known punished before this report: a punish already said, or one
        // resumed to ask again. A punish in this run goes from refunding
        // straight to asking the seller for the NEX, with no outcome yet;
        // one resumed asking reports that first, maybe before the engine
        // says it was punished, and is not said again.
        let resumed_asking = status["step"] == "asking"
            && !(self.last.get(&id)).is_some_and(|last| last["step"] != "asking");
        let punished = self.punished.contains(&id) || resumed_asking;
        let notice = notice(status, punished);
        if status["outcome"] == "punished" || status["step"] == "asking" {
            self.punished.insert(id.clone());
        }
        self.last.insert(id.clone(), status.clone());
        notice.filter(|notice| self.notified.insert((id, *notice)))
    }

    /// What the engine holds now: whether a swap needs the app, if that has
    /// changed. Statuses of swaps no longer held are dropped. Nothing is
    /// decided while the engine cannot say (a reopen).
    fn look(&mut self, activity: &Activity) -> Option<bool> {
        self.last.retain(|id, _| activity.held.contains(id));
        self.punished.extend(activity.punished.iter().cloned());
        self.followed = activity.held.first().cloned();
        let stored = activity.stored?;
        let needs = needs_app(activity.busy || stored, &self.last);
        (self.needs != Some(needs)).then(|| {
            self.needs = Some(needs);
            needs
        })
    }

    fn line(&self) -> String {
        status_line(self.followed.as_ref().and_then(|id| self.last.get(id)))
    }
}

/// Launch at login, as tauri-plugin-autostart sets it (behind a trait for
/// tests).
pub trait LoginLaunch {
    fn is_enabled(&self) -> Result<bool>;
    fn enable(&self) -> Result<()>;
    fn disable(&self) -> Result<()>;
}

impl LoginLaunch for AutoLaunchManager {
    fn is_enabled(&self) -> Result<bool> {
        AutoLaunchManager::is_enabled(self).map_err(err)
    }
    fn enable(&self) -> Result<()> {
        AutoLaunchManager::enable(self).map_err(err)
    }
    fn disable(&self) -> Result<()> {
        AutoLaunchManager::disable(self).map_err(err)
    }
}

/// Launch at login on while a swap `needs` the app, off once none does. The
/// plugin cannot say who turned it on, so `marker` records that this app
/// did: one already on without it is the user's, and is left alone. The
/// marker is written before enabling, so a crash between the two leaves it
/// to be turned off at the next launch.
fn launch_at_login(launch: &dyn LoginLaunch, marker: &Path, needs: bool) -> Result<()> {
    let ours = marker.exists();
    if needs {
        if ours || launch.is_enabled()? {
            return Ok(());
        }
        if let Some(dir) = marker.parent() {
            fs::create_dir_all(dir).map_err(err)?;
        }
        fs::write(marker, b"").map_err(err)?;
        if let Err(e) = launch.enable() {
            let _ = fs::remove_file(marker);
            return Err(e);
        }
    } else if ours {
        launch.disable()?;
        fs::remove_file(marker).map_err(err)?;
    }
    Ok(())
}

/// How often a failed enable of launch at login is tried again.
const LAUNCH_RETRY: Duration = Duration::from_secs(60);

/// A failed launch-at-login enable, to try again.
#[derive(Default)]
struct LaunchRetry {
    /// When the last try failed, while a swap needed the app.
    failed: Option<Instant>,
    /// Its failure has been logged: logged once, until one succeeds.
    logged: bool,
}

/// [`launch_at_login`] at a look at `now`: when the decision `changed`, and
/// again at most each [`LAUNCH_RETRY`] while a swap `needs` the app and the
/// last try failed. The error to log, the first of a run of failures only.
fn launch_tick(
    launch: &dyn LoginLaunch,
    marker: &Path,
    changed: Option<bool>,
    needs: bool,
    retry: &mut LaunchRetry,
    now: Instant,
) -> Option<String> {
    let due = needs && retry.failed.is_some_and(|at| now - at >= LAUNCH_RETRY);
    if changed.is_none() && !due {
        return None;
    }
    match launch_at_login(launch, marker, needs) {
        Ok(()) => {
            *retry = LaunchRetry::default();
            None
        }
        Err(e) => {
            retry.failed = needs.then_some(now);
            (!std::mem::replace(&mut retry.logged, true)).then_some(e)
        }
    }
}

fn marker_path(app: &AppHandle) -> Result<PathBuf> {
    Ok(app_root(app)?.join("swap").join("launch-at-login"))
}

/// The app's background state, as the app manages it.
#[derive(Default)]
pub struct Background {
    /// Whether a swap needs the app: read by the window and exit handlers,
    /// which must not wait on the engine.
    needs: AtomicBool,
    /// The window was hidden on close: shown again only by the user.
    hidden: AtomicBool,
    /// A quit confirm is on screen.
    asking: AtomicBool,
    /// The user confirmed a quit: the exit goes through.
    confirmed: AtomicBool,
    /// Launch at login failed to turn on: tried again.
    retry_launch: Mutex<LaunchRetry>,
    tracker: Mutex<Tracker>,
    /// The tray and its status line, built the first time a swap needs the
    /// app; None if it could not be built.
    tray: OnceLock<Option<(TrayIcon, MenuItem<Wry>)>>,
}

/// What closing the window does.
#[derive(Debug, PartialEq)]
enum Close {
    /// Closes, as it always has: no swap needs the app.
    Go,
    /// Hides the window, to be shown again from the tray.
    Hide,
    /// Asks first, as a quit does: no tray to show the window again from.
    Confirm,
}

fn on_close(needs: bool, tray: bool) -> Close {
    match (needs, tray) {
        (false, _) => Close::Go,
        (true, true) => Close::Hide,
        (true, false) => Close::Confirm,
    }
}

/// Whether an exit waits for the user's confirm.
fn holds_exit(needs: bool, confirmed: bool) -> bool {
    needs && !confirmed
}

/// The ids of the tray's and the app menu's items.
const OPEN: &str = "open";
const QUIT: &str = "quit";

/// The background state, and the thread that watches the engine. Managed
/// before any swap can report.
pub fn start(app: &AppHandle) -> Result<()> {
    app.manage(Background::default());
    #[cfg(target_os = "macos")]
    if let Err(e) = terminate::install(app) {
        eprintln!("lattice-desktop: the Dock's Quit is not held: {e}");
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("swap background".into())
        .spawn(move || loop {
            look(&app);
            std::thread::sleep(TICK);
        })
        .map_err(err)?;
    Ok(())
}

/// The tray: Open, the swap's status line, Quit.
fn make_tray(app: &AppHandle) -> Result<(TrayIcon, MenuItem<Wry>)> {
    let status = MenuItem::new(app, "Swap in progress", false, None::<&str>).map_err(err)?;
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, OPEN, "Open Lattice", true, None::<&str>).map_err(err)?,
            &status,
            &PredefinedMenuItem::separator(app).map_err(err)?,
            &MenuItem::with_id(app, QUIT, "Quit Lattice", true, None::<&str>).map_err(err)?,
        ],
    )
    .map_err(err)?;
    let mut tray = TrayIconBuilder::with_id("swap")
        .menu(&menu)
        .tooltip("Lattice: swap in progress");
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    Ok((tray.build(app).map_err(err)?, status))
}

/// Builds the tray on the main thread, once. On Linux, building it panics
/// when no appindicator library is installed: then there is no tray, and a
/// close asks first instead of hiding the window.
fn build_tray(app: &AppHandle) {
    let app = app.clone();
    let built = app.clone().run_on_main_thread(move || {
        let background = app.state::<Background>();
        if background.tray.get().is_some() {
            return;
        }
        let tray = catch_unwind(AssertUnwindSafe(|| make_tray(&app)))
            .unwrap_or_else(|_| Err("building it panicked".into()));
        let tray = match tray {
            Ok((tray, status)) => {
                // The swap may have stopped needing the app meanwhile.
                let _ = tray.set_visible(background.needs.load(Ordering::SeqCst));
                Some((tray, status))
            }
            Err(e) => {
                eprintln!("lattice-desktop: no tray: {e}");
                None
            }
        };
        let _ = background.tray.set(tray);
    });
    if let Err(e) = built {
        eprintln!("lattice-desktop: no tray: {e}");
    }
}

/// A look at the engine: the tray, launch at login and a hidden window
/// follow whether a swap needs the app.
fn look(app: &AppHandle) {
    let activity = match crate::swap::activity(app) {
        Ok(activity) => activity,
        Err(e) => return eprintln!("lattice-desktop: swap activity: {e}"),
    };
    let background = app.state::<Background>();
    let (changed, needs, line) = {
        let Ok(mut tracker) = background.tracker.lock() else {
            return;
        };
        let changed = tracker.look(&activity);
        (changed, tracker.needs == Some(true), tracker.line())
    };
    if changed.is_some() {
        background.needs.store(needs, Ordering::SeqCst);
    }
    match background.tray.get() {
        Some(Some((tray, status))) => {
            let _ = status.set_text(&line);
            let _ = tray.set_visible(needs);
        }
        Some(None) => {}
        None if needs => build_tray(app),
        None => {}
    }
    let failed = match (marker_path(app), background.retry_launch.lock()) {
        (Ok(marker), Ok(mut retry)) => launch_tick(
            &*app.autolaunch(),
            &marker,
            changed,
            needs,
            &mut retry,
            Instant::now(),
        ),
        (Err(e), _) => Some(e),
        (_, Err(e)) => Some(err(e)),
    };
    if let Some(e) = failed {
        eprintln!("lattice-desktop: launch at login: {e}");
    }
    // The window was closed while a swap needed the app: that close now
    // goes through.
    if changed == Some(false) && background.hidden.load(Ordering::SeqCst) {
        app.exit(0);
    }
}

/// A swap's status report: its notice, if one is due, and the tray's line.
pub fn report(app: &AppHandle, status: &Value) {
    let Some(background) = app.try_state::<Background>() else {
        return;
    };
    let notice = match background.tracker.lock() {
        Ok(mut tracker) => tracker.report(status),
        Err(_) => return,
    };
    if let Some(notice) = notice {
        let (title, body) = notice.text();
        if let Err(e) = app.notification().builder().title(title).body(body).show() {
            eprintln!("lattice-desktop: notification: {e}");
        }
    }
}

/// Shows the main window: the tray's Open, the dock, a second launch.
pub fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    // A second launch can come before setup has managed the state.
    if let Some(background) = app.try_state::<Background>() {
        background.hidden.store(false, Ordering::SeqCst);
    }
}

/// Whether closing the window is held: while a swap needs the app, the
/// window is hidden to the tray, or with no tray the close asks first, as a
/// quit does.
pub fn hold_close(window: &tauri::Window) -> bool {
    let background = window.state::<Background>();
    let tray = matches!(background.tray.get(), Some(Some(_)));
    match on_close(background.needs.load(Ordering::SeqCst), tray) {
        Close::Go => false,
        Close::Hide => {
            background.hidden.store(true, Ordering::SeqCst);
            let _ = window.hide();
            true
        }
        Close::Confirm => hold_exit(window.app_handle()),
    }
}

/// Whether an exit must wait for the user: a swap needs the app and no quit
/// was confirmed. If so, the confirm is asked; a confirmed one exits.
pub fn hold_exit(app: &AppHandle) -> bool {
    let background = app.state::<Background>();
    if !holds_exit(
        background.needs.load(Ordering::SeqCst),
        background.confirmed.load(Ordering::SeqCst),
    ) {
        return false;
    }
    if background.asking.swap(true, Ordering::SeqCst) {
        return true;
    }
    // The dialog is a sheet on the window (rfd): on a hidden one it would
    // not be seen, and the quit would wait on it unanswered.
    show(app);
    let warning = match background.tracker.lock() {
        Ok(tracker) => quit_warning(tracker.last.values(), now()),
        Err(_) => quit_warning(std::iter::empty(), now()),
    };
    let app = app.clone();
    app.dialog()
        .message(warning)
        .title("Quit Lattice during a swap?")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Quit".into(),
            "Keep Running".into(),
        ))
        .show(move |quit| {
            let background = app.state::<Background>();
            background.asking.store(false, Ordering::SeqCst);
            if quit {
                background.confirmed.store(true, Ordering::SeqCst);
                app.exit(0);
            }
        });
    true
}

/// The tray's and the app menu's items.
pub fn menu_event(app: &AppHandle, id: &str) {
    match id {
        OPEN => show(app),
        // Through `ExitRequested`, which asks first while a swap needs the
        // app.
        QUIT => app.exit(0),
        _ => {}
    }
}

/// The macOS app menu, Tauri's default with its Quit (Cmd+Q) made one that
/// raises `ExitRequested`: the predefined one terminates at once.
#[cfg(target_os = "macos")]
pub fn app_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    use tauri::menu::MenuItemKind;
    let menu = Menu::default(app)?;
    if let Some(MenuItemKind::Submenu(first)) = menu.items()?.first() {
        for item in first.items()? {
            if let MenuItemKind::Predefined(predefined) = &item {
                if predefined.text()?.starts_with("Quit") {
                    first.remove(predefined)?;
                }
            }
        }
        let name = &app.package_info().name;
        first.append(&MenuItem::with_id(
            app,
            QUIT,
            format!("Quit {name}"),
            true,
            Some("CmdOrCtrl+Q"),
        )?)?;
    }
    Ok(menu)
}

/// The Dock's Quit, a logout and a restart send `terminate:`, which tao
/// lets through at once (its app delegate has no
/// `applicationShouldTerminate:`). This adds that method to tao's delegate,
/// as an AppKit app holds a quit: `NSTerminateCancel` while the exit is
/// held, the confirm then asked; a confirmed quit exits as Quit does.
#[cfg(target_os = "macos")]
mod terminate {
    use std::sync::OnceLock;

    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{class, msg_send, sel};
    use tauri::AppHandle;

    use crate::Result;

    static APP: OnceLock<AppHandle> = OnceLock::new();

    /// `NSApplicationTerminateReply`.
    const CANCEL: usize = 0;
    const NOW: usize = 1;

    extern "C-unwind" fn should_terminate(_: &AnyObject, _: Sel, _: *mut AnyObject) -> usize {
        match APP.get() {
            Some(app) if super::hold_exit(app) => CANCEL,
            _ => NOW,
        }
    }

    /// Adds `applicationShouldTerminate:` to the app delegate's class. On
    /// the main thread, once the event loop has set the delegate (setup).
    /// Written against tao 0.37, whose delegate has no such method:
    /// `class_addMethod` refuses (and this says so) if a later tao adds one.
    pub fn install(app: &AppHandle) -> Result<()> {
        let _ = APP.set(app.clone());
        unsafe {
            let ns_app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let delegate: *mut AnyObject = msg_send![ns_app, delegate];
            let delegate = delegate.as_ref().ok_or("no app delegate")?;
            let imp: Imp = std::mem::transmute(
                should_terminate as extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject) -> usize,
            );
            let class = delegate.class() as *const AnyClass as *mut AnyClass;
            let added = objc2::ffi::class_addMethod(
                class,
                sel!(applicationShouldTerminate:),
                imp,
                c"Q@:@".as_ptr(),
            );
            if !added.as_bool() {
                return Err("the app delegate already answers applicationShouldTerminate:".into());
            }
        }
        Ok(())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |now| now.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    fn status(id: &str, step: &str, outcome: Option<&str>, funding: Option<u64>) -> Value {
        json!({
            "swapId": id,
            "step": step,
            "outcome": outcome,
            "confirmations": { "funding": funding },
            "btc_confirmations": 3,
            "mustStayOpen": false,
            "deadlines": { "cancel": null, "refund": null, "punish": null },
        })
    }

    fn activity(busy: bool, held: &[&str], punished: &[&str]) -> Activity {
        Activity {
            busy,
            held: held.iter().map(|id| id.to_string()).collect(),
            punished: punished.iter().map(|id| id.to_string()).collect(),
            stored: Some(false),
        }
    }

    #[test]
    fn a_stored_swap_needs_the_app_and_a_reopen_decides_nothing() {
        // At launch, before the node runs and the wallet opens: a swap on
        // disk to resume needs the app; the first look does not say no.
        let mut tracker = Tracker::default();
        let before_open = Activity {
            stored: Some(true),
            ..Default::default()
        };
        assert_eq!(tracker.look(&before_open), Some(true));
        // Mid-reopen the engine holds nothing: nothing is decided.
        let reopening = Activity {
            stored: None,
            ..Default::default()
        };
        assert_eq!(tracker.look(&reopening), None);
        assert_eq!(tracker.needs, Some(true));
        assert_eq!(
            tracker.look(&activity(true, &["a"], &[])),
            None,
            "held again"
        );
        assert_eq!(tracker.look(&activity(false, &[], &[])), Some(false));
        // Not known at the first look: undecided, not no.
        let mut tracker = Tracker::default();
        assert_eq!(tracker.look(&reopening), None);
        assert_eq!(tracker.needs, None);
    }

    #[test]
    fn the_page_gets_no_tray_permission() {
        // `core:default` takes in `core:tray:default` with the `tray-icon`
        // feature; the tray is the shell's own.
        let capability: Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        let permissions = capability["permissions"].as_array().unwrap();
        assert!(permissions.iter().all(|p| {
            let p = p.as_str().unwrap();
            p != "core:default" && !p.starts_with("core:tray")
        }));
        assert!(permissions.contains(&json!("core:event:default")));
    }

    #[test]
    fn a_close_hides_to_the_tray_or_with_none_asks_first() {
        assert_eq!(on_close(false, true), Close::Go);
        assert_eq!(on_close(false, false), Close::Go);
        assert_eq!(on_close(true, true), Close::Hide);
        assert_eq!(
            on_close(true, false),
            Close::Confirm,
            "never hidden with no way back"
        );
        assert!(holds_exit(true, false));
        assert!(!holds_exit(true, true), "a confirmed quit goes through");
        assert!(!holds_exit(false, false));
    }

    #[test]
    fn a_swap_needs_the_app_while_a_run_goes_or_it_must_stay_open() {
        let mut last = HashMap::new();
        assert!(!needs_app(false, &last), "nothing held");
        assert!(needs_app(true, &last), "a run going");
        last.insert("a".into(), status("a", "done", Some("swapped"), Some(3)));
        assert!(!needs_app(false, &last), "ended");
        let mut open = status("a", "finishing", None, Some(3));
        open["mustStayOpen"] = json!(true);
        last.insert("a".into(), open);
        assert!(needs_app(false, &last), "must stay open");

        // Decided from the engine: a change is reported once, and a swap
        // no longer held no longer counts.
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.look(&activity(false, &[], &[])),
            Some(false),
            "the first look decides"
        );
        assert_eq!(tracker.look(&activity(false, &[], &[])), None);
        assert_eq!(tracker.look(&activity(true, &["a"], &[])), Some(true));
        let mut open = status("a", "finishing", None, Some(3));
        open["mustStayOpen"] = json!(true);
        tracker.report(&open);
        assert_eq!(
            tracker.look(&activity(false, &["a"], &[])),
            None,
            "still must stay open"
        );
        assert_eq!(
            tracker.look(&activity(false, &[], &[])),
            Some(false),
            "no longer held"
        );
    }

    #[test]
    fn notices_come_once_per_swap_and_step() {
        let mut tracker = Tracker::default();
        let mut seen = vec![];
        for report in [
            status("a", "fund-wallet", None, None),
            status("a", "swapping", None, None),
            status("a", "swapping", None, Some(2)),
            status("a", "swapping", None, Some(3)),
            status("a", "swapping", None, Some(4)),
            status("a", "finishing", None, Some(5)),
            status("a", "refunding", None, Some(9)),
            status("a", "refunding", None, Some(10)),
            status("a", "done", Some("refunded"), None),
            status("a", "done", Some("refunded"), None),
            // Another swap's own notices.
            status("b", "swapping", None, Some(3)),
            // Received once the claim is in a block, not again once it ends.
            status("b", "received", None, Some(9)),
            status("b", "done", Some("swapped"), None),
            status("b", "done", Some("swapped"), None),
            status("c", "cancelled", None, None),
            status("c", "done", Some("aborted"), None),
        ] {
            seen.extend(
                tracker
                    .report(&report)
                    .map(|n| (report["swapId"].clone(), n)),
            );
        }
        assert_eq!(
            seen,
            [
                (json!("a"), Notice::Locked),
                (json!("a"), Notice::Refunding),
                (json!("a"), Notice::Refunded),
                (json!("b"), Notice::Locked),
                (json!("b"), Notice::Received),
            ]
        );
    }

    #[test]
    fn a_punished_swaps_recovery_is_its_own_notice() {
        // Resumed punished: known from the engine, its lock confirmed long ago.
        let mut tracker = Tracker::default();
        tracker.look(&activity(true, &["p"], &["p"]));
        assert_eq!(
            tracker.report(&status("p", "finishing", None, Some(30))),
            None
        );
        assert_eq!(
            tracker.report(&status("p", "received", None, Some(31))),
            Some(Notice::Recovered)
        );
        assert_eq!(
            tracker.report(&status("p", "done", Some("swapped"), None)),
            None
        );
        // Punished in this run: refunding goes straight to asking, said
        // once as punished, and the NEX that follows is recovered.
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.report(&status("r", "refunding", None, Some(80))),
            Some(Notice::Refunding)
        );
        assert_eq!(
            tracker.report(&status("r", "asking", None, None)),
            Some(Notice::Punished)
        );
        assert_eq!(tracker.report(&status("r", "asking", None, None)), None);
        assert_eq!(tracker.report(&status("r", "finishing", None, None)), None);
        assert_eq!(
            tracker.report(&status("r", "received", None, None)),
            Some(Notice::Recovered)
        );
        // Resumed asking, reported before the engine says it was punished:
        // not said again, and its NEX is recovered.
        let mut tracker = Tracker::default();
        assert_eq!(tracker.report(&status("s", "asking", None, None)), None);
        assert_eq!(
            tracker.report(&status("s", "received", None, None)),
            Some(Notice::Recovered)
        );
        // Punished in this session: said once.
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.report(&status("q", "done", Some("punished"), None)),
            Some(Notice::Punished)
        );
        assert_eq!(
            tracker.report(&status("q", "done", Some("punished"), None)),
            None
        );
        // Resumed punished at a later start: not said again.
        let mut tracker = Tracker::default();
        tracker.look(&activity(true, &["q"], &["q"]));
        assert_eq!(
            tracker.report(&status("q", "done", Some("punished"), None)),
            None
        );
        assert_eq!(
            tracker.report(&status("r", "done", Some("drained"), None)),
            None
        );
    }

    #[test]
    fn the_tray_shows_the_followed_swaps_line() {
        assert_eq!(
            status_line(Some(&status("a", "swapping", None, Some(1)))),
            "Swap: BTC lock 1 of 3 confirmations"
        );
        assert_eq!(
            status_line(Some(&status("a", "refunding", None, None))),
            "Swap: refunding your BTC"
        );
        assert_eq!(
            status_line(Some(&status("a", "asking", None, None))),
            "Swap punished: asking the seller for the NEX"
        );
        let mut received = status("a", "received", None, Some(6));
        assert_eq!(status_line(Some(&received)), "Swap: NEX received");
        received["confirmations"]["nexLock"] = json!(1);
        assert_eq!(
            status_line(Some(&received)),
            "Swap: NEX received · 1 Nexus confirmation"
        );
        received["confirmations"]["nexLock"] = json!(30);
        assert_eq!(
            status_line(Some(&received)),
            "Swap: NEX received · 30 Nexus confirmations"
        );
        assert_eq!(status_line(None), "Swap in progress");
        let mut tracker = Tracker::default();
        tracker.report(&status("p", "finishing", None, None));
        tracker.report(&status("a", "fund-wallet", None, None));
        tracker.look(&activity(true, &["a", "p"], &["p"]));
        assert_eq!(tracker.line(), "Swap: waiting for your BTC deposit");
    }

    #[test]
    fn the_quit_confirm_names_the_earliest_refund_deadline() {
        let hour = 3_600_000;
        let mut a = status("a", "refunding", None, None);
        a["deadlines"]["punish"] = json!({ "height": 900_100, "eta": 10 * hour });
        let mut b = status("b", "finishing", None, None);
        b["deadlines"]["punish"] = json!({ "height": 900_050, "eta": 5 * hour });
        let warning = quit_warning([&a, &b].into_iter(), hour);
        assert!(
            warning.contains("before Bitcoin block 900050 (in about 4 hours)"),
            "{warning}"
        );
        let none = quit_warning([&status("c", "fund-wallet", None, None)].into_iter(), 0);
        assert!(
            none.contains("it stops until you open Lattice again"),
            "{none}"
        );
    }

    /// Launch at login as the OS would keep it.
    #[derive(Default)]
    struct FakeLaunch {
        on: RefCell<bool>,
        fail: Cell<bool>,
    }

    impl LoginLaunch for FakeLaunch {
        fn is_enabled(&self) -> Result<bool> {
            Ok(*self.on.borrow())
        }
        fn enable(&self) -> Result<()> {
            if self.fail.get() {
                return Err("refused".into());
            }
            *self.on.borrow_mut() = true;
            Ok(())
        }
        fn disable(&self) -> Result<()> {
            *self.on.borrow_mut() = false;
            Ok(())
        }
    }

    fn marker(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lattice-login-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("swap").join("launch-at-login")
    }

    #[test]
    fn launch_at_login_is_on_only_while_a_swap_needs_the_app() {
        let marker = marker("ours");
        let launch = FakeLaunch::default();
        launch_at_login(&launch, &marker, false).unwrap();
        assert!(!launch.is_enabled().unwrap(), "never on otherwise");
        launch_at_login(&launch, &marker, true).unwrap();
        assert!(launch.is_enabled().unwrap() && marker.exists());
        launch_at_login(&launch, &marker, true).unwrap();
        assert!(launch.is_enabled().unwrap());
        launch_at_login(&launch, &marker, false).unwrap();
        assert!(!launch.is_enabled().unwrap() && !marker.exists());

        // Left on by a crash: off at the next launch's first look.
        launch_at_login(&launch, &marker, true).unwrap();
        let relaunched = FakeLaunch {
            on: RefCell::new(true),
            fail: Cell::new(false),
        };
        launch_at_login(&relaunched, &marker, false).unwrap();
        assert!(!relaunched.is_enabled().unwrap());

        // An enable that fails leaves no marker behind.
        let failing = FakeLaunch {
            fail: Cell::new(true),
            ..Default::default()
        };
        assert!(launch_at_login(&failing, &marker, true).is_err());
        assert!(!marker.exists());
        fs::remove_dir_all(marker.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_enable_is_tried_again_each_minute_while_needed() {
        let marker = marker("retry");
        let launch = FakeLaunch {
            fail: Cell::new(true),
            ..Default::default()
        };
        let mut retry = LaunchRetry::default();
        let t0 = Instant::now();
        let tick = |retry: &mut LaunchRetry, changed, needs, at| {
            launch_tick(&launch, &marker, changed, needs, retry, at)
        };
        assert_eq!(
            tick(&mut retry, Some(true), true, t0),
            Some("refused".into()),
            "logged"
        );
        // Not before a minute has passed.
        launch.fail.set(false);
        assert_eq!(
            tick(&mut retry, None, true, t0 + Duration::from_secs(59)),
            None
        );
        assert!(!launch.is_enabled().unwrap(), "not tried yet");
        // Then tried; still failing, not logged again.
        launch.fail.set(true);
        assert_eq!(tick(&mut retry, None, true, t0 + LAUNCH_RETRY), None);
        assert_eq!(retry.failed, Some(t0 + LAUNCH_RETRY), "tried");
        launch.fail.set(false);
        assert_eq!(tick(&mut retry, None, true, t0 + 2 * LAUNCH_RETRY), None);
        assert!(launch.is_enabled().unwrap());
        assert!(retry.failed.is_none() && !retry.logged, "reset");
        // A new run of failures is logged again; no longer needed, no retry.
        launch.fail.set(true);
        *launch.on.borrow_mut() = false;
        let _ = fs::remove_file(&marker);
        assert!(tick(&mut retry, Some(true), true, t0).is_some());
        assert_eq!(tick(&mut retry, Some(false), false, t0), None);
        assert!(retry.failed.is_none());
        fs::remove_dir_all(marker.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn launch_at_login_the_user_turned_on_is_left_on() {
        let marker = marker("users");
        let launch = FakeLaunch {
            on: RefCell::new(true),
            fail: Cell::new(false),
        };
        launch_at_login(&launch, &marker, true).unwrap();
        assert!(!marker.exists(), "not ours");
        launch_at_login(&launch, &marker, false).unwrap();
        assert!(launch.is_enabled().unwrap(), "the user's, kept");
    }
}
