// Runs inside KWin for the life of one rox process. It reports where rox's
// windows sit and applies the moves and pins rox asks for, since a Wayland
// client can do neither itself. kwin.rs loads it and prepends SERVICE (rox's
// bus name), OBJECT_PATH and INTERFACE (where its bridge answers), PID, and
// PLUGIN (this script's name in KWin).
//
// rox's commands arrive as the reply to a Next call that rox holds open until
// it has something to send, so the script never needs a D-Bus name of its own.

// rox takes a window's tag back off after this, so a bind still waiting
// can't match anymore.
const BIND_MS = 10000;

// Past D-Bus's own 25 s call timeout, so a poll this old has failed.
const POLL_STALE_MS = 30000;

// Stale polls in a row before deciding rox is gone and unloading.
const MAX_MISSES = 3;

const entries = [];
const pending = [];

function ours(win) {
    return win.pid === PID && win.normalWindow;
}

function report(entry) {
    const win = entry.win;
    if (entry.key === null || win.move || win.resize) {
        return;
    }

    const g = win.frameGeometry;
    callDBus(SERVICE, OBJECT_PATH, INTERFACE, "Frame", JSON.stringify({
        key: entry.key,
        x: g.x,
        y: g.y,
        above: win.keepAbove,
    }));
}

function track(win) {
    if (!ours(win)) {
        return;
    }

    const entry = { win: win, key: null };
    entries.push(entry);

    win.frameGeometryChanged.connect(() => report(entry));
    win.interactiveMoveResizeFinished.connect(() => report(entry));
    win.keepAboveChanged.connect(() => report(entry));
    // The tag can land after the window maps.
    win.captionChanged.connect(() => matchPending(entry));
    win.closed.connect(() => {
        const at = entries.indexOf(entry);
        if (at >= 0) {
            entries.splice(at, 1);
        }
    });

    matchPending(entry);
}

// A bind that arrived before its window mapped or took its tag.
function matchPending(entry) {
    if (entry.key !== null) {
        return;
    }

    const now = Date.now();
    while (pending.length && now - pending[0].at > BIND_MS) {
        pending.shift();
    }
    const at = pending.findIndex((p) => entry.win.caption.includes(p.cmd.tag));
    if (at >= 0) {
        claim(entry, pending.splice(at, 1)[0].cmd);
    }
}

// Only the top strip has to land on a screen, so the window can still be
// grabbed and dragged back after a monitor goes away.
function onScreen(rect) {
    return workspace.screens.some((screen) => {
        const s = screen.geometry;
        return rect.x < s.x + s.width && rect.x + rect.width > s.x
            && rect.y < s.y + s.height && rect.y + 32 > s.y;
    });
}

function apply(win, cmd) {
    if (typeof cmd.above === "boolean") {
        win.keepAbove = cmd.above;
    }
    if (typeof cmd.x !== "number" || typeof cmd.y !== "number") {
        return;
    }

    // rox sends the client size it just resized to. Setting the frame at the
    // old size would race that resize and snap the window back.
    const g = win.frameGeometry;
    const c = win.clientGeometry;
    const rect = {
        x: cmd.x,
        y: cmd.y,
        width: typeof cmd.w === "number" ? cmd.w + (g.width - c.width) : g.width,
        height: typeof cmd.h === "number" ? cmd.h + (g.height - c.height) : g.height,
    };
    if (onScreen(rect)) {
        win.frameGeometry = rect;
    }
}

function claim(entry, cmd) {
    entry.key = cmd.key;
    apply(entry.win, cmd);
    report(entry);
}

// rox tags the workspace window's title before sending this, since a pop-out
// or dialog from the same pid is otherwise indistinguishable. No match yet
// means the window hasn't mapped or its new caption hasn't reached KWin.
function bind(cmd) {
    for (const entry of entries) {
        if (entry.key === null && entry.win.caption.includes(cmd.tag)) {
            claim(entry, cmd);
            return;
        }
    }

    pending.push({ cmd: cmd, at: Date.now() });
}

function place(cmd) {
    for (const entry of entries) {
        if (entry.key === cmd.key) {
            apply(entry.win, cmd);
            return;
        }
    }
}

let generation = 0;
let pollStarted = 0;
let misses = 0;

function poll() {
    const mine = ++generation;
    pollStarted = Date.now();
    callDBus(SERVICE, OBJECT_PATH, INTERFACE, "Next", (reply) => {
        // Commands on a superseded poll were still taken off rox's queue.
        for (const cmd of JSON.parse(reply)) {
            if (cmd.op === "bind") {
                bind(cmd);
            } else {
                place(cmd);
            }
        }
        if (mine !== generation) {
            return;
        }

        pollStarted = 0;
        misses = 0;
        poll();
    });
}

// A failed call never reaches its callback, so polling would just stop.
const watchdog = new QTimer();
watchdog.interval = 10000;
watchdog.timeout.connect(() => {
    if (pollStarted === 0 || Date.now() - pollStarted < POLL_STALE_MS) {
        return;
    }

    misses += 1;
    if (misses >= MAX_MISSES) {
        watchdog.stop();
        callDBus("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", PLUGIN);
        return;
    }
    poll();
});
watchdog.start();

workspace.windowAdded.connect(track);
workspace.windowList().forEach(track);
poll();
