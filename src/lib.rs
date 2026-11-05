// SPDX-License-Identifier: GPL-3.0-or-later
//! Canvas: boards of notes, files, links and groups joined by arrows, in
//! the JSON Canvas format -- the files Obsidian's Canvas reads and writes.
//!
//! Open a `.canvas` file and the board comes up; the file's JSON is one
//! key away (`TAB`). The drawing is Forge's canvas (`canvas` in the plugin
//! interface): this plugin owns the board, Forge draws it and tells the
//! plugin what was clicked.
//!
//! Mouse: click selects, drag moves (a group takes what it holds), drag the
//! square at a selected card's corner to resize, double-click empty space
//! for a new note, double-click a card to edit or open it, wheel zooms,
//! middle-drag pans.
//!
//! Keys: `n` note, `f` file, `u` link, `G` group, `e` edit, `d` delete,
//! `c` connect, `C` colour, `0` fit, `s` snap, `TAB` JSON, `C-x C-s` save.
//!
//! Settings (init.rhai): `forge.plugin.config("canvas", #{ auto_view: true,
//! vault: "~/Notes" });`. `vault` is where file cards are looked for (default:
//! the folder with `.obsidian` above the canvas, else the canvas's folder).
//! To show file cards' text, grant the plugin the vault folder:
//! `forge.plugin.grant("canvas", #{ directories: ["~/Notes"] });`
//!
//!   cargo build --release --target wasm32-wasip2
//!   cp target/wasm32-wasip2/release/forge_canvas_plugin.wasm \
//!      ~/.config/forge/plugins/canvas.wasm

use std::cell::RefCell;
use std::collections::HashMap;

use forge_notes::jsoncanvas::{Action, Board, Canvas, Event as PointerEvent, Files, Pointer, Sel, scene};
use forge_plugin_sdk::{
    Buffer, EventKind, MenuItem, Position, Span, bind_in_mode, canvas, command, completing_read,
    define_menu_for_mode, files, forge_plugin, menu_item, menu_separator, message, modes, on,
    read_string, run_command,
};
use serde_json::Value;

const MODE: &str = "json-canvas-mode";
const VIEW_MODE: &str = "json-canvas-view-mode";
const FRAME: (f32, f32) = (1200.0, 800.0);

struct Session {
    board: Board,
    pointer: Pointer,
    /// The `.canvas` file's buffer.
    source: u32,
    root: String,
    /// The source's version after our own last write: a different one means
    /// the user edited the JSON.
    wrote_version: u64,
}

thread_local! {
    static SESSIONS: RefCell<HashMap<String, Session>> = RefCell::new(HashMap::new());
    static NEXT: RefCell<u32> = const { RefCell::new(0) };
    static CONFIG: RefCell<Config> = RefCell::new(Config::default());
}

#[derive(Default, Clone)]
struct Config {
    auto_view: bool,
    vault: Option<String>,
}

fn parse_config(json: &str) -> Result<Config, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("canvas: settings: {e}"))?;
    let map = v.as_object().ok_or("canvas: settings must be a table")?;
    let mut c = Config { auto_view: true, vault: None };
    for (k, v) in map {
        match k.as_str() {
            "auto_view" => c.auto_view = v.as_bool().ok_or("canvas: auto_view must be true or false")?,
            "vault" => c.vault = Some(v.as_str().ok_or("canvas: vault must be text")?.to_string()),
            other => return Err(format!("canvas: unknown setting {other} (auto_view, vault)")),
        }
    }
    Ok(c)
}

/// The vault a canvas file belongs to: `vault` from the settings, else the
/// nearest folder above it with `.obsidian`, else its own folder.
fn vault_root(path: &str) -> String {
    if let Some(v) = CONFIG.with(|c| c.borrow().vault.clone()) {
        return v.trim_end_matches('/').to_string();
    }
    // The nearest folder above the file holding `.obsidian`, else its own.
    match files::locate_dominating_file(path, ".obsidian") {
        Some(dir) => files::directory_name(&dir),
        None => files::directory_name(&files::name_directory(path).unwrap_or_default()),
    }
}

struct VaultFiles(String);

impl Files for VaultFiles {
    fn resolve(&self, file: &str) -> Option<String> {
        let p = format!("{}/{}", self.0, file.trim_start_matches('/'));
        files::exists(&p).then_some(p)
    }

    fn preview(&self, file: &str) -> Option<String> {
        let text = files::read(&self.resolve(file)?).ok()?;
        let body = text.strip_prefix("---\n").and_then(|r| r.split_once("\n---\n").map(|(_, b)| b)).unwrap_or(&text);
        Some(body.lines().filter(|l| !l.trim().is_empty()).take(12).collect::<Vec<_>>().join("\n"))
    }
}

fn whole(buffer: &Buffer) -> Span {
    let _ = buffer;
    Span { start: Position { line: 0, column: 0 }, end: Position { line: u32::MAX, column: u32::MAX } }
}


/// Scenes name colour roles (`card`, `line` ...); Forge picks the colours
/// when it draws them, so a theme or light/dark change recolours every
/// picture at once. Nothing to fetch from the theme here.
fn apply_theme() {
    forge_canvas::Palette::set_current(forge_canvas::Palette::tokens());
}


fn draw(name: &str) {
    apply_theme();
    let json = SESSIONS.with(|s| {
        let s = s.borrow();
        let ses = s.get(name)?;
        Some(scene(&ses.board, FRAME.0, FRAME.1, &VaultFiles(ses.root.clone())).to_json())
    });
    if let Some(json) = json {
        if let Err(e) = canvas::show(name, &json) {
            message(&format!("canvas: {e}"));
        }
    }
}

/// Write the board back into the file's buffer.
fn sync_source(name: &str) {
    SESSIONS.with(|s| {
        let mut s = s.borrow_mut();
        let Some(ses) = s.get_mut(name) else { return };
        let Some(b) = Buffer::from_id(ses.source).filter(Buffer::alive) else { return };
        b.replace(whole(&b), &ses.board.canvas.to_text());
        ses.wrote_version = b.version();
    });
}

fn changed(name: &str) {
    sync_source(name);
    draw(name);
}

fn current_name() -> Option<String> {
    // The canvas buffer is named *Canvas: <name>*.
    let cur = Buffer::current().name();
    let n = cur.strip_prefix("*Canvas: ")?.strip_suffix('*')?.to_string();
    SESSIONS.with(|s| s.borrow().contains_key(&n)).then_some(n)
}

fn with_board<R>(f: impl FnOnce(&mut Board) -> R) -> Option<R> {
    let name = current_name()?;
    SESSIONS.with(|s| s.borrow_mut().get_mut(&name).map(|ses| f(&mut ses.board)))
}

fn centre(name: &str) -> (f32, f32) {
    let (x, y, z) = canvas::get_view(name).unwrap_or((0.0, 0.0, 1.0));
    (x + FRAME.0 / z / 2.0, y + FRAME.1 / z / 2.0)
}

fn handle_event(name: &str) {
    let Some(json) = canvas::last_event(name) else { return };
    let Ok(ev) = serde_json::from_str::<PointerEvent>(&json) else { return };
    let actions = SESSIONS.with(|s| {
        let mut s = s.borrow_mut();
        let ses = s.get_mut(name)?;
        let (board, pointer) = (&mut ses.board, &mut ses.pointer);
        Some(pointer.handle(board, &ev))
    });
    for a in actions.unwrap_or_default() {
        match a {
            Action::Redraw => draw(name),
            Action::Changed => sync_source(name),
            Action::Edit(id) => edit(name, &id),
            Action::OpenFile(f) => open_file(name, &f),
            Action::OpenUrl(u) => message(&format!("Link: {u}")),
            Action::Message(m) => message(&m),
        }
    }
}

fn edit(name: &str, id: &str) {
    let (prompt, initial) = SESSIONS.with(|s| {
        let s = s.borrow();
        let Some(ses) = s.get(name) else { return ("Text: ".to_string(), String::new()) };
        match ses.board.canvas.node(id) {
            Some(n) => match n.kind.as_str() {
                "group" => ("Group label: ".into(), n.label.clone().unwrap_or_default()),
                "link" => ("URL: ".into(), n.url.clone().unwrap_or_default()),
                "file" => ("File: ".into(), n.file.clone().unwrap_or_default()),
                _ => ("Note: ".into(), n.text.clone().unwrap_or_default()),
            },
            None => ("Edge label: ".into(), ses.board.canvas.edges.iter().find(|e| e.id == id).and_then(|e| e.label.clone()).unwrap_or_default()),
        }
    });
    let (name, id) = (name.to_string(), id.to_string());
    read_string(&prompt, &initial, move |answer| {
        if let Some(text) = answer {
            SESSIONS.with(|s| {
                if let Some(ses) = s.borrow_mut().get_mut(&name) {
                    ses.board.set_text(&id, &text);
                }
            });
            changed(&name);
        }
    });
}

fn open_file(name: &str, file: &str) {
    let root = SESSIONS.with(|s| s.borrow().get(name).map(|x| x.root.clone())).unwrap_or_default();
    match Buffer::visit(&format!("{root}/{file}")) {
        Ok(b) => b.show(),
        Err(e) => message(&format!("{file}: {e}")),
    }
}

/// Show the board for a `.canvas` buffer.
fn open_view(src: &Buffer) -> Result<(), String> {
    let path = src.path().unwrap_or_else(|| src.name());
    let name = format!("canvas:{path}");
    let canvas_data = Canvas::parse(&src.text())?;
    let existing = SESSIONS.with(|s| s.borrow().contains_key(&name));
    SESSIONS.with(|s| {
        s.borrow_mut().insert(
            name.clone(),
            Session { board: Board::new(canvas_data), pointer: Pointer::default(), source: src.id(), root: vault_root(&path), wrote_version: src.version() },
        );
    });
    let fit = SESSIONS.with(|s| s.borrow().get(&name).map(|x| x.board.fit(FRAME.0, FRAME.1)));
    let buffer = canvas::show(&name, &SESSIONS.with(|s| {
        let s = s.borrow();
        let ses = &s[&name];
        scene(&ses.board, FRAME.0, FRAME.1, &VaultFiles(ses.root.clone())).to_json()
    }))?;
    if !existing {
        let n = NEXT.with(|n| {
            *n.borrow_mut() += 1;
            *n.borrow()
        });
        let cmd = format!("json-canvas-event-{n}");
        let for_name = name.clone();
        command(&cmd, "Handle the pointer on a canvas board (internal).", move |_| {
            handle_event(&for_name);
            Ok(())
        })?;
        canvas::on_event(&name, &cmd)?;
    }
    if let Some((x, y, z)) = fit {
        canvas::set_view(&name, x, y, z)?;
    }
    buffer.set_major_mode(VIEW_MODE)?;
    canvas::display(&name)?;
    Ok(())
}

fn source_of_view() -> Option<Buffer> {
    let name = current_name()?;
    SESSIONS.with(|s| s.borrow().get(&name).map(|x| x.source)).and_then(Buffer::from_id).filter(Buffer::alive)
}

fn ask_then(prompt: &str, initial: &str, f: impl FnOnce(&str, &str) + 'static) {
    let Some(name) = current_name() else {
        message("Not in a canvas board");
        return;
    };
    let name2 = name.clone();
    read_string(prompt, initial, move |a| {
        if let Some(text) = a.filter(|t| !t.is_empty()) {
            f(&name2, &text);
        }
    });
}

fn add(kind: &'static str) -> Result<(), String> {
    let prompt = match kind {
        "note" => "Note: ",
        "file" => "File (relative to the vault): ",
        "link" => "URL: ",
        _ => "Group label: ",
    };
    ask_then(prompt, "", move |name, text| {
        let (x, y) = centre(name);
        SESSIONS.with(|s| {
            if let Some(ses) = s.borrow_mut().get_mut(name) {
                match kind {
                    "note" => ses.board.add_text(x, y, text),
                    "file" => ses.board.add_file(x, y, text),
                    "link" => ses.board.add_link(x, y, text),
                    _ => ses.board.add_group(x, y, text),
                };
            }
        });
        changed(name);
    });
    Ok(())
}

fn edit_selected() -> Result<(), String> {
    let name = current_name().ok_or("Not in a canvas board")?;
    let sel = SESSIONS.with(|s| s.borrow().get(&name).and_then(|x| x.board.selected.clone()));
    match sel {
        Some(Sel::Node(id) | Sel::Edge(id)) => edit(&name, &id),
        None => message("Select something first (click it)"),
    }
    Ok(())
}

fn delete_selected() -> Result<(), String> {
    let name = current_name().ok_or("Not in a canvas board")?;
    let done = with_board(Board::delete_selected).unwrap_or(false);
    if done {
        changed(&name);
    } else {
        message("Nothing selected");
    }
    Ok(())
}

fn connect() -> Result<(), String> {
    let name = current_name().ok_or("Not in a canvas board")?;
    let ok = with_board(|b| match &b.selected {
        Some(Sel::Node(id)) => {
            b.connect_from = Some(id.clone());
            true
        }
        _ => false,
    })
    .unwrap_or(false);
    if ok {
        draw(&name);
    } else {
        message("Select a node to connect from, then run connect");
    }
    Ok(())
}

fn cancel() -> Result<(), String> {
    if let Some(name) = current_name() {
        with_board(|b| b.connect_from = None);
        draw(&name);
    }
    Ok(())
}

fn colour() -> Result<(), String> {
    let name = current_name().ok_or("Not in a canvas board")?;
    let choices: Vec<String> = ["none", "red", "orange", "yellow", "green", "cyan", "purple"].iter().map(|s| s.to_string()).collect();
    completing_read("Colour: ", &choices, false, move |a| {
        let Some(a) = a else { return };
        let preset = match a.as_str() {
            "none" => None,
            "red" => Some("1".to_string()),
            "orange" => Some("2".into()),
            "yellow" => Some("3".into()),
            "green" => Some("4".into()),
            "cyan" => Some("5".into()),
            "purple" => Some("6".into()),
            hex if hex.starts_with('#') => Some(hex.to_string()),
            _ => return message("Pick a colour name or #rrggbb"),
        };
        SESSIONS.with(|s| {
            if let Some(ses) = s.borrow_mut().get_mut(&name) {
                if let Some(Sel::Node(id) | Sel::Edge(id)) = ses.board.selected.clone() {
                    ses.board.set_color(&id, preset.as_deref());
                }
            }
        });
        changed(&name);
    });
    Ok(())
}

fn fit() -> Result<(), String> {
    let name = current_name().ok_or("Not in a canvas board")?;
    if let Some((x, y, z)) = with_board(|b| b.fit(FRAME.0, FRAME.1)) {
        canvas::set_view(&name, x, y, z)?;
    }
    Ok(())
}

fn toggle_snap() -> Result<(), String> {
    if let Some(on) = with_board(|b| {
        b.snap = !b.snap;
        b.snap
    }) {
        message(if on { "Snap to grid on" } else { "Snap to grid off" });
    }
    Ok(())
}

fn show_source() -> Result<(), String> {
    match source_of_view() {
        Some(b) => {
            b.show();
            Ok(())
        }
        None => Err("This board has no source file".into()),
    }
}

/// The file's JSON was edited by hand: take it.
fn reload(src: &Buffer) {
    let id = src.id();
    let found = SESSIONS.with(|s| s.borrow().iter().find(|(_, x)| x.source == id).map(|(n, x)| (n.clone(), x.wrote_version)));
    let Some((name, wrote)) = found else { return };
    if src.version() == wrote {
        return;
    }
    if let Ok(c) = Canvas::parse(&src.text()) {
        SESSIONS.with(|s| {
            if let Some(ses) = s.borrow_mut().get_mut(&name) {
                ses.board.canvas = c;
                ses.board.selected = None;
                ses.wrote_version = src.version();
            }
        });
        draw(&name);
    }
}

fn save() -> Result<(), String> {
    let src = source_of_view().ok_or("Not in a canvas board")?;
    let name = current_name().ok_or("Not in a canvas board")?;
    sync_source(&name);
    src.show();
    run_command("save-buffer")?;
    run_command("json-canvas-view")?;
    Ok(())
}

fn new_board() -> Result<(), String> {
    read_string("New canvas file: ", "~/", |a| {
        let Some(path) = a.filter(|p| !p.is_empty()) else { return };
        let path = if path.ends_with(".canvas") { path } else { format!("{path}.canvas") };
        match Buffer::visit(&path) {
            Ok(b) => {
                if b.text().trim().is_empty() {
                    b.replace(whole(&b), &Canvas::default().to_text());
                }
                b.show();
                if let Err(e) = open_view(&b) {
                    message(&format!("canvas: {e}"));
                }
            }
            Err(e) => message(&format!("{path}: {e}")),
        }
    });
    Ok(())
}

fn view_current() -> Result<(), String> {
    let b = Buffer::current();
    if current_name().is_some() {
        return Ok(());
    }
    open_view(&b)
}

fn close_view() -> Result<(), String> {
    if let Some(name) = current_name() {
        SESSIONS.with(|s| s.borrow_mut().remove(&name));
        canvas::close(&name);
    }
    Ok(())
}

fn init() -> Result<(), String> {
    let cfg = parse_config(&forge_plugin_sdk::config_json())?;
    CONFIG.with(|c| *c.borrow_mut() = cfg.clone());

    modes::define_major_mode(MODE, Some("fundamental-mode"), &["canvas".to_string()])?;
    modes::define_major_mode(VIEW_MODE, Some("canvas-mode"), &[])?;

    command("json-canvas-view", "Show this .canvas file as a board.", |_| view_current())?;
    command("json-canvas-new", "Make a new .canvas file and open its board.", |_| new_board())?;
    command("json-canvas-source", "Show the JSON behind this board (TAB).", |_| show_source())?;
    command("json-canvas-add-note", "Add a text card to the board.", |_| add("note"))?;
    command("json-canvas-add-file", "Add a card for a note or picture in the vault.", |_| add("file"))?;
    command("json-canvas-add-link", "Add a web link card.", |_| add("link"))?;
    command("json-canvas-add-group", "Add a labelled group.", |_| add("group"))?;
    command("json-canvas-edit", "Edit the selected card's text, or an arrow's label.", |_| edit_selected())?;
    command("json-canvas-delete", "Delete the selected card or arrow.", |_| delete_selected())?;
    command("json-canvas-connect", "Draw an arrow from the selected card to the next one clicked.", |_| connect())?;
    command("json-canvas-cancel", "Cancel connecting.", |_| cancel())?;
    command("json-canvas-color", "Colour the selected card or arrow.", |_| colour())?;
    command("json-canvas-fit", "Zoom to show the whole board.", |_| fit())?;
    command("json-canvas-snap", "Toggle snapping to the grid.", |_| toggle_snap())?;
    command("json-canvas-save", "Save the board's file.", |_| save())?;
    command("json-canvas-close", "Close this board (the file stays).", |_| close_view())?;
    command("json-canvas-export", "Save the board as a PNG or SVG picture.", |_| run_command("canvas-export"))?;

    for (keys, cmd) in [
        ("n", "json-canvas-add-note"),
        ("f", "json-canvas-add-file"),
        ("u", "json-canvas-add-link"),
        ("G", "json-canvas-add-group"),
        ("e", "json-canvas-edit"),
        ("RET", "json-canvas-edit"),
        ("d", "json-canvas-delete"),
        ("DEL", "json-canvas-delete"),
        ("c", "json-canvas-connect"),
        ("C", "json-canvas-color"),
        ("0", "json-canvas-fit"),
        ("s", "json-canvas-snap"),
        ("TAB", "json-canvas-source"),
        ("C-x C-s", "json-canvas-save"),
        ("C-g", "json-canvas-cancel"),
        ("ESC", "json-canvas-cancel"),
        ("w", "json-canvas-export"),
    ] {
        bind_in_mode(VIEW_MODE, keys, cmd)?;
    }
    bind_in_mode(MODE, "C-c C-c", "json-canvas-view")?;

    let items: Vec<MenuItem> = vec![
        menu_item("Add note", "json-canvas-add-note"),
        menu_item("Add file", "json-canvas-add-file"),
        menu_item("Add link", "json-canvas-add-link"),
        menu_item("Add group", "json-canvas-add-group"),
        menu_separator(),
        menu_item("Edit", "json-canvas-edit"),
        menu_item("Connect", "json-canvas-connect"),
        menu_item("Colour…", "json-canvas-color"),
        menu_item("Delete", "json-canvas-delete"),
        menu_separator(),
        menu_item("Fit to window", "json-canvas-fit"),
        menu_item("Toggle snap", "json-canvas-snap"),
        menu_item("Show JSON", "json-canvas-source"),
        menu_item("Export picture…", "json-canvas-export"),
        menu_item("Save", "json-canvas-save"),
    ];
    define_menu_for_mode("Canvas", &items, VIEW_MODE)?;
    define_menu_for_mode("Canvas", &[menu_item("Show as board", "json-canvas-view"), menu_item("New board…", "json-canvas-new")], MODE)?;

    // Opening a .canvas file shows its board.
    if cfg.auto_view {
        let open = |e: &forge_plugin_sdk::Event| {
            let id = forge_plugin_sdk::event_buffer(e);
            if let Some(b) = Buffer::from_id(id).filter(Buffer::alive)
                && b.major_mode() == MODE
                && Buffer::current().id() == id
                && b.name().ends_with(".canvas")
                && !SESSIONS.with(|s| s.borrow().values().any(|x| x.source == id))
                && let Err(e) = open_view(&b)
            {
                message(&format!("canvas: {e}"));
            }
        };
        on(EventKind::BufferOpened, open);
        on(EventKind::ModeChanged, open);
    }
    // Hand edits to the JSON show up on the board.
    on(EventKind::BufferChanged, |e| {
        if let Some(b) = Buffer::from_id(forge_plugin_sdk::event_buffer(e)).filter(Buffer::alive) {
            reload(&b);
        }
    });
    Ok(())
}

forge_plugin!(init);
