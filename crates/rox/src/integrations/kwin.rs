//! Window placement on KWin under Wayland, where a client can neither move
//! itself nor ask to stay on top. rox loads `kwin.js` into KWin through its
//! `/Scripting` D-Bus interface; the script reports where each workspace
//! window sits and applies the moves and pins rox sends back.
//!
//! Trust: the script runs inside the compositor with reach over every window
//! on the desktop, so it's a fixed template with nothing spliced in but rox's
//! own bus name, pid, script name and bridge address. The bridge answers
//! KWin's connection only. Outside a sandbox any process in the session can
//! load a script the same way, so this grants nothing rox didn't already
//! have. A Flatpak would need the `org.kde.KWin` talk grant, which hands the
//! sandbox the compositor, so `placement` never starts this there.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, bail};
use gpui::{AnyWindowHandle, App, Global, Pixels, Point, Size, Window, point, px};
use rox_panel_api::windows;
use serde::{Deserialize, Serialize};

/// The bridge's address on rox's own bus connection. A D-Bus object path,
/// not a file.
const OBJECT_PATH: &str = "/com/zealsprince/rox/KWin";

/// zbus wants a literal in `#[interface]`, so the test below holds the two
/// together.
const INTERFACE: &str = "com.zealsprince.rox.KWin";

const KWIN: &str = "org.kde.KWin";

/// A held Next is answered empty after this, inside D-Bus's 25 s call timeout.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// How long a window's title carries its tag before rox gives up on the
/// script finding it. The script drops its side of the bind after the same.
const UNCLAIMED: Duration = Duration::from_secs(10);

/// A window is keyed by its gpui id, which the script echoes back.
#[derive(Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Command {
    /// Claim the window whose caption carries the tag, then place it.
    Bind(Bind),
    Place(Place),
}

#[derive(Serialize)]
struct Bind {
    tag: String,
    #[serde(flatten)]
    place: Place,
}

/// Unset fields are left as they are. `w` and `h` are the content size a
/// resize just asked for, which KWin would otherwise undo.
#[derive(Serialize)]
struct Place {
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    x: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    y: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    w: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    h: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    above: Option<bool>,
}

/// KWin's frame position, in its logical coordinates.
#[derive(Deserialize)]
struct Frame {
    key: String,
    x: f32,
    y: f32,
    above: bool,
}

enum Event {
    /// The script polled, so it's running.
    Live,
    Frame(Frame),
}

/// `None` on the queue is the keepalive, answering a held Next empty.
#[derive(Default)]
struct Kwin {
    commands: Option<async_channel::Sender<Option<Command>>>,
    live: bool,
    frames: HashMap<u64, Frame>,
    /// Binds held until the script is up. Tagging a title before then
    /// would leave the tag showing on a compositor that never runs it.
    waiting: Vec<(AnyWindowHandle, Place)>,
    /// Windows whose title carries a tag the script hasn't answered yet.
    tagged: HashMap<u64, AnyWindowHandle>,
}

impl Global for Kwin {}

/// Connect, load the script, and keep it loaded until quit. Quietly does
/// nothing when KWin isn't on the session bus.
pub(crate) fn start(cx: &mut App) {
    let (commands, queue) = async_channel::unbounded();
    let (events, inbox) = async_channel::unbounded();
    let (quit, quit_rx) = async_channel::bounded::<async_channel::Sender<()>>(1);
    cx.set_global(Kwin {
        commands: Some(commands.clone()),
        ..Default::default()
    });

    cx.background_executor()
        .spawn(serve(queue, events, quit_rx))
        .detach();

    cx.spawn(async move |cx| {
        while let Ok(event) = inbox.recv().await {
            if cx.update(|cx| handle(event, cx)).is_err() {
                break;
            }
        }
    })
    .detach();

    // Only while the queue is empty, so a script that never came up can't
    // make it grow.
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(KEEPALIVE).await;
            if commands.is_closed() {
                break;
            }
            if commands.is_empty() {
                let _ = commands.try_send(None);
            }
        }
    })
    .detach();

    // KWin keeps a loaded script until it's told otherwise.
    cx.on_app_quit(move |_| {
        let quit = quit.clone();
        async move {
            let (done, landed) = async_channel::bounded(1);
            if quit.send(done).await.is_ok() {
                let _ = landed.recv().await;
            }
        }
    })
    .detach();
}

/// Whether the script is up, which is what makes moving and pinning work.
pub(crate) fn live(cx: &App) -> bool {
    cx.try_global::<Kwin>().is_some_and(|kwin| kwin.live)
}

/// Where KWin last reported the window's frame.
pub(crate) fn origin(window: &Window, cx: &App) -> Option<Point<Pixels>> {
    let frame = cx.try_global::<Kwin>()?.frames.get(&key(window))?;
    Some(point(px(frame.x), px(frame.y)))
}

pub(crate) fn above(window: &Window, cx: &App) -> Option<bool> {
    let frame = cx.try_global::<Kwin>()?.frames.get(&key(window))?;
    Some(frame.above)
}

/// Tie a just-opened window to its KWin counterpart, placing it on the way.
pub(crate) fn bind(
    window: &mut Window,
    origin: Option<Point<Pixels>>,
    above: Option<bool>,
    cx: &mut App,
) {
    let place = place_for(window, origin, None, above);
    let kwin = cx.default_global::<Kwin>();
    if kwin.live {
        tag_and_bind(window, place, cx);
    } else {
        kwin.waiting.push((window.window_handle(), place));
    }
}

/// Every window of rox's pid looks alike to KWin, pop-outs and dialogs
/// included, so the script tells this one apart by a tag on its caption.
fn tag_and_bind(window: &mut Window, place: Place, cx: &mut App) {
    let key = key(window);
    let tag = format!(" [rox:{key}]");
    windows::set_title_tag(window, Some(tag.clone()));
    cx.default_global::<Kwin>()
        .tagged
        .insert(key, window.window_handle());
    send(Command::Bind(Bind { tag, place }), cx);

    cx.spawn(async move |cx| {
        cx.background_executor().timer(UNCLAIMED).await;
        let _ = cx.update(|cx| untag(key, cx));
    })
    .detach();
}

/// Put the plain title back once the script has the window, or has given
/// up on it.
fn untag(key: u64, cx: &mut App) {
    let Some(handle) = cx.default_global::<Kwin>().tagged.remove(&key) else {
        return;
    };
    let _ = handle.update(cx, |_, window, _| windows::set_title_tag(window, None));
}

pub(crate) fn place(
    window: &Window,
    origin: Option<Point<Pixels>>,
    size: Option<Size<Pixels>>,
    above: Option<bool>,
    cx: &App,
) {
    let place = place_for(window, origin, size, above);
    send(Command::Place(place), cx);
}

pub(crate) fn forget(window: &Window, cx: &mut App) {
    if cx.has_global::<Kwin>() {
        let key = key(window);
        let kwin = cx.global_mut::<Kwin>();
        kwin.frames.remove(&key);
        kwin.tagged.remove(&key);
        kwin.waiting
            .retain(|(handle, _)| handle.window_id().as_u64() != key);
    }
}

fn key(window: &Window) -> u64 {
    window.window_handle().window_id().as_u64()
}

fn place_for(
    window: &Window,
    origin: Option<Point<Pixels>>,
    size: Option<Size<Pixels>>,
    above: Option<bool>,
) -> Place {
    Place {
        key: key(window).to_string(),
        x: origin.map(|o| o.x.into()),
        y: origin.map(|o| o.y.into()),
        w: size.map(|s| s.width.into()),
        h: size.map(|s| s.height.into()),
        above,
    }
}

fn send(command: Command, cx: &App) {
    if let Some(commands) = cx
        .try_global::<Kwin>()
        .and_then(|kwin| kwin.commands.as_ref())
    {
        let _ = commands.try_send(Some(command));
    }
}

fn handle(event: Event, cx: &mut App) {
    let kwin = cx.default_global::<Kwin>();
    let repaint = match event {
        Event::Live => {
            if kwin.live {
                return;
            }
            kwin.live = true;

            for (handle, place) in std::mem::take(&mut kwin.waiting) {
                let _ = handle.update(cx, |_, window, cx| tag_and_bind(window, place, cx));
            }
            true
        }

        Event::Frame(frame) => {
            let Ok(key) = frame.key.parse::<u64>() else {
                return;
            };
            let above = frame.above;
            let was = kwin.frames.insert(key, frame).map(|old| old.above);

            // A report means the script has the window, so the tag it was
            // found by can go.
            untag(key, cx);
            was != Some(above)
        }
    };

    // The pin button reads both, so it has to redraw when either moves.
    if repaint {
        cx.refresh_windows();
    }
}

async fn serve(
    queue: async_channel::Receiver<Option<Command>>,
    events: async_channel::Sender<Event>,
    quit: async_channel::Receiver<async_channel::Sender<()>>,
) {
    let (conn, plugin) = match connect(queue, events).await {
        Ok(up) => up,
        Err(err) => {
            log::info!("kwin: no window placement: {err:#}");
            return;
        }
    };
    log::info!("kwin: placement script loaded as {plugin}");

    if let Ok(done) = quit.recv().await {
        let unloaded = conn
            .call_method(
                Some(KWIN),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "unloadScript",
                &(plugin.as_str(),),
            )
            .await;
        if let Err(err) = unloaded {
            log::warn!("kwin: placement script left loaded: {err}");
        }
        let _ = done.send(()).await;
    }
}

async fn connect(
    queue: async_channel::Receiver<Option<Command>>,
    events: async_channel::Sender<Event>,
) -> anyhow::Result<(zbus::Connection, String)> {
    let conn = zbus::Connection::session().await?;
    let kwin: String = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &(KWIN,),
        )
        .await
        .context("KWin isn't on the session bus")?
        .body()
        .deserialize()?;

    let bridge = Bridge {
        queue,
        events,
        kwin,
        polled: AtomicBool::new(false),
    };
    conn.object_server().at(OBJECT_PATH, bridge).await?;

    // Spliced as JSON strings, so nothing in them can break out of the literal.
    let service = conn.unique_name().context("no bus name")?.to_string();
    let pid = std::process::id();
    let plugin = format!("rox-{pid}");
    let source = format!(
        "const SERVICE = {};\nconst OBJECT_PATH = {};\nconst INTERFACE = {};\n\
         const PID = {pid};\nconst PLUGIN = {};\n\n{}",
        serde_json::to_string(&service)?,
        serde_json::to_string(OBJECT_PATH)?,
        serde_json::to_string(INTERFACE)?,
        serde_json::to_string(&plugin)?,
        include_str!("kwin.js"),
    );

    // KWin reads the script from a file, so it goes in the per-user runtime
    // dir and comes out once the script has run.
    let path = dirs::runtime_dir()
        .context("no runtime dir")?
        .join(format!("{plugin}.js"));
    std::fs::write(&path, source)?;
    let loaded = load(&conn, &path, &plugin).await;
    let _ = std::fs::remove_file(&path);
    loaded?;

    Ok((conn, plugin))
}

async fn load(conn: &zbus::Connection, path: &std::path::Path, plugin: &str) -> anyhow::Result<()> {
    let path = path.to_str().context("runtime dir isn't UTF-8")?;
    let load_script = async || -> anyhow::Result<i32> {
        let id = conn
            .call_method(
                Some(KWIN),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "loadScript",
                &(path, plugin),
            )
            .await?
            .body()
            .deserialize()?;
        Ok(id)
    };

    // -1 is a script already loaded under this name: one left behind by a
    // crashed rox that had the same pid.
    let mut id = load_script().await?;
    if id < 0 {
        conn.call_method(
            Some(KWIN),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            "unloadScript",
            &(plugin,),
        )
        .await?;
        id = load_script().await?;
    }
    if id < 0 {
        bail!("KWin wouldn't load the script");
    }

    conn.call_method(
        Some(KWIN),
        format!("/Scripting/Script{id}").as_str(),
        Some("org.kde.kwin.Script"),
        "run",
        &(),
    )
    .await?;
    Ok(())
}

struct Bridge {
    queue: async_channel::Receiver<Option<Command>>,
    events: async_channel::Sender<Event>,
    /// KWin's unique name, the only caller answered.
    kwin: String,
    polled: AtomicBool,
}

impl Bridge {
    /// Next would hand rox's queue to anyone who asked, so everything but
    /// KWin's own connection is turned away.
    fn check_caller(&self, header: &zbus::message::Header<'_>) -> zbus::fdo::Result<()> {
        if header
            .sender()
            .is_some_and(|sender| sender.as_str() == self.kwin)
        {
            Ok(())
        } else {
            Err(zbus::fdo::Error::AccessDenied("not KWin".into()))
        }
    }
}

#[zbus::interface(name = "com.zealsprince.rox.KWin")]
impl Bridge {
    /// Held until there's something to send, then answered with every
    /// queued command as a JSON array.
    async fn next(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<String> {
        self.check_caller(&header)?;
        if !self.polled.swap(true, Ordering::Relaxed) {
            let _ = self.events.send(Event::Live).await;
        }

        let mut commands = Vec::new();
        if let Ok(first) = self.queue.recv().await {
            commands.extend(first);
        }
        while let Ok(more) = self.queue.try_recv() {
            commands.extend(more);
        }
        serde_json::to_string(&commands).map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    async fn frame(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        frame: String,
    ) -> zbus::fdo::Result<()> {
        self.check_caller(&header)?;
        match serde_json::from_str(&frame) {
            Ok(frame) => {
                let _ = self.events.send(Event::Frame(frame)).await;
            }
            Err(err) => log::debug!("kwin: unreadable frame report: {err}"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::object_server::Interface;

    #[test]
    fn interface_matches_the_spliced_name() {
        assert_eq!(Bridge::name().as_str(), INTERFACE);
    }
}
